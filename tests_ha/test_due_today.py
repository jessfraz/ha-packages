"""One local-day projection drives calendar and the expected/OFD union."""

import json
from datetime import date, datetime
from uuid import UUID

import pytest
from homeassistant.core import HomeAssistant

from custom_components.shipments.models import DeliveryDay, delivery_estimate

from .conftest import FixtureShipment, get_state


@pytest.mark.parametrize(
    ("point", "now", "expected"),
    [
        ("2026-10-28", "2026-10-28T07:00:00+00:00", True),
        ("2026-10-28T00:00:00Z", "2026-10-28T07:00:00+00:00", False),
        ("2026-10-29T03:00:00Z", "2026-10-28T07:00:00+00:00", True),
        ("2026-10-28T06:59:59Z", "2026-10-28T07:00:00+00:00", False),
        ("2026-03-09T06:59:59Z", "2026-03-08T10:00:00+00:00", True),
        ("2026-03-09T07:00:00Z", "2026-03-08T10:00:00+00:00", False),
        ("2026-11-02T07:59:59Z", "2026-11-01T10:00:00+00:00", True),
        ("2026-11-02T08:00:00Z", "2026-11-01T10:00:00+00:00", False),
    ],
)
async def test_due_today_uses_calendar_bounds(
    hass: HomeAssistant, point, now, expected
):
    await hass.config.async_set_time_zone("America/Los_Angeles")
    _, estimate = delivery_estimate(
        {
            "id": str(UUID(int=1)),
            "title": "Synthetic estimate",
            "status": "in_transit",
            "estimated_delivery_at": point,
        }
    )
    assert estimate is not None
    assert (
        estimate.due_on(
            DeliveryDay.at(datetime.fromisoformat(now), "America/Los_Angeles")
        )
        is expected
    )


async def test_due_today_union_is_deduplicated_and_preserves_statuses(
    hass, entry, backend, freezer
):
    await hass.config.async_set_time_zone("America/Los_Angeles")
    freezer.move_to(datetime.fromisoformat("2026-10-28T19:00:00+00:00"))
    await hass.async_block_till_done(wait_background_tasks=True)
    rows = [
        FixtureShipment(
            id=str(UUID(int=1)), title="On vehicle", estimated_delivery="2026-10-28"
        ),
        FixtureShipment(
            id=str(UUID(int=2)),
            title="Expected",
            status="in_transit",
            estimated_delivery="2026-10-29T03:00:00Z",
        ),
        FixtureShipment(id=str(UUID(int=3)), title="Undated on vehicle"),
        FixtureShipment(
            id=str(UUID(int=4)),
            title="Tomorrow",
            status="in_transit",
            estimated_delivery="2026-10-29",
        ),
    ]
    backend.shipment_rows = rows
    estimates = [
        {
            "id": row.id,
            "title": row.title,
            "carrier": row.carrier,
            "direction": "inbound",
            "status": row.status,
            "estimated_delivery_at": row.estimated_delivery,
        }
        for row in rows
        if row.estimated_delivery
    ]
    backend.responses[("GET", "/v1/delivery-estimates")] = (
        200,
        json.dumps({"items": estimates, "next_page": None}).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    state = get_state(hass, "sensor.shipments_inbound_due_today")
    assert state.state == "3"
    assert {item["id"] for item in state.attributes["items"]} == {
        str(UUID(int=i)) for i in (1, 2, 3)
    }
    assert {item["status"] for item in state.attributes["items"]} == {
        "in_transit",
        "out_for_delivery",
    }
    assert get_state(hass, "sensor.shipments_inbound_out_for_delivery").state == "2"
    expected = next(
        item
        for item in entry.runtime_data.inbound_estimates
        if str(item.shipment_id) == str(UUID(int=2))
    )
    assert expected.calendar_event().start == date(2026, 10, 28)


async def test_paired_window_uses_overlap_not_point(hass):
    await hass.config.async_set_time_zone("America/Los_Angeles")
    _, estimate = delivery_estimate(
        {
            "id": str(UUID(int=1)),
            "title": "Window",
            "status": "in_transit",
            "estimated_delivery_at": None,
            "estimated_delivery_window": {
                "start": "2026-10-28T23:00:00-07:00",
                "end": "2026-10-29T01:00:00-07:00",
            },
        }
    )
    assert estimate is not None
    for day in ("2026-10-28", "2026-10-29"):
        assert estimate.due_on(
            DeliveryDay.at(
                datetime.fromisoformat(day + "T12:00:00-07:00"), "America/Los_Angeles"
            )
        )
    assert estimate.calendar_event().start == datetime.fromisoformat(
        "2026-10-28T23:00:00-07:00"
    )


async def test_due_today_deduplicates_across_ofd_pages(hass, entry, backend, freezer):
    await hass.config.async_set_time_zone("America/Los_Angeles")
    freezer.move_to(datetime.fromisoformat("2026-10-28T19:00:00+00:00"))
    await hass.async_block_till_done(wait_background_tasks=True)
    rows = [
        FixtureShipment(id=str(UUID(int=i)), title=f"Parcel {i:02}")
        for i in range(1, 23)
    ]
    backend.shipment_rows = rows
    # Status changed between estimates and the fully paginated status read.
    stale = {
        "id": rows[-1].id,
        "title": rows[-1].title,
        "carrier": "USPS",
        "status": "in_transit",
        "estimated_delivery_at": "2026-10-28",
    }
    backend.responses[("GET", "/v1/delivery-estimates")] = (
        200,
        json.dumps({"items": [stale], "next_page": None}).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    state = get_state(hass, "sensor.shipments_inbound_due_today")
    assert state.state == "22"
    assert len(state.attributes["items"]) == 20
    assert state.attributes["items_truncated"] is True
    assert all(
        item["status"] == "out_for_delivery" for item in state.attributes["items"]
    )
    assert any(
        request.query.get("page_token") == "20"
        for request in backend.requests
        if request.path == "/v1/shipments"
    )
