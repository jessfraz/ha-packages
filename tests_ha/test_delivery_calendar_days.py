"""Delivery-day visibility and truthful delivered history across local days."""

from datetime import date, datetime, timedelta
from uuid import UUID
from zoneinfo import ZoneInfo

import pytest
from homeassistant.core import HomeAssistant

from custom_components.shipments.models import delivery_estimate

SHIPMENT = "0193c69f-e5c4-7e93-b6cb-8172dacb617f"


@pytest.mark.parametrize(
    ("raw", "local_day", "hours"),
    [
        ("2026-10-26T18:00:00Z", "2026-10-26", 24),
        ("2026-10-27T00:00:00Z", "2026-10-26", 24),
        ("2026-10-27T07:00:00Z", "2026-10-27", 24),
        ("2026-03-08T10:30:00Z", "2026-03-08", 23),
        ("2026-11-01T09:30:00Z", "2026-11-01", 25),
        ("2026-03-08", "2026-03-08", 23),
        ("2026-11-01", "2026-11-01", 25),
    ],
)
async def test_estimates_remain_visible_through_local_midnight(
    hass: HomeAssistant, raw: str, local_day: str, hours: int
) -> None:
    await hass.config.async_set_time_zone("America/Los_Angeles")
    identity, projection = delivery_estimate(
        {
            "id": SHIPMENT,
            "title": "Synthetic parcel",
            "status": "in_transit",
            "estimated_delivery_at": raw,
        }
    )
    assert identity == UUID(SHIPMENT)
    assert projection is not None
    event = projection.calendar_event()
    day = date.fromisoformat(local_day)
    assert event.start == day
    assert event.end == day + timedelta(days=1)
    assert event.description is not None
    assert "Carrier estimate:" in event.description
    assert projection.estimated.isoformat() in event.description
    # Dates follow the civil day, not a fixed 24-hour window across DST.
    start = datetime.combine(day, datetime.min.time(), ZoneInfo("America/Los_Angeles"))
    end = datetime.combine(
        event.end, datetime.min.time(), ZoneInfo("America/Los_Angeles")
    )
    assert (end.timestamp() - start.timestamp()) / 3600 == hours
    assert event.end_datetime_local > end - timedelta(seconds=1)


@pytest.mark.parametrize("actual", ["2026-10-26T18:00:00Z", "2026-10-26"])
async def test_delivered_history_uses_actual_day_and_is_explicit(
    hass: HomeAssistant, actual: str
) -> None:
    await hass.config.async_set_time_zone("America/Los_Angeles")
    _, projection = delivery_estimate(
        {
            "id": SHIPMENT,
            "title": "Synthetic parcel",
            "status": "delivered",
            "estimated_delivery_at": "2026-10-25",
            "delivered_at": actual,
        }
    )
    assert projection is not None
    event = projection.calendar_event()
    assert event.start == date(2026, 10, 26)
    assert event.end == date(2026, 10, 27)
    assert event.summary == "Delivered: Synthetic parcel"
    assert event.description is not None
    assert "Actual delivery:" in event.description
    assert "Carrier estimates can change" not in event.description


@pytest.mark.parametrize(
    "extra",
    [
        {"status": "delivered", "delivered_at": None},
        {"status": "delivered", "delivered_at": "invalid"},
        {"status": "cancelled"},
        {"status": "return_to_sender"},
        {"archived": True},
        {"direction": "outbound"},
    ],
)
def test_no_fabricated_delivery_dates_or_reopened_terminal_items(extra: dict) -> None:
    _, projection = delivery_estimate(
        {
            "id": SHIPMENT,
            "title": "Synthetic parcel",
            "status": "in_transit",
            "estimated_delivery_at": "2026-10-26",
            **extra,
        }
    )
    assert projection is None
