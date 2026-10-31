"""Typed provider dates retain civil precision through native HA projections."""

from datetime import date, datetime

import pytest
from homeassistant.core import HomeAssistant

from custom_components.shipments.models import (
    DeliveryDay,
    ShipmentItem,
    delivery_estimate,
)

from .conftest import SHIPMENT_ID, FixtureShipment


def report(kind: str, **fields: object) -> dict[str, object]:
    return {
        "kind": kind,
        "observed_at": "2026-10-31T19:00:00Z",
        "timezone": "America/Los_Angeles",
        **fields,
    }


@pytest.mark.parametrize("delivered", [False, True])
async def test_typed_point_dates_become_all_day_events_without_fake_instants(
    hass: HomeAssistant, delivered: bool
) -> None:
    await hass.config.async_set_time_zone("America/Los_Angeles")
    _, projection = delivery_estimate(
        {
            "id": SHIPMENT_ID,
            "title": "Civil precision",
            "status": "delivered" if delivered else "in_transit",
            "delivery_date": report(
                "delivered" if delivered else "estimated", date="2026-11-01"
            ),
        }
    )
    assert projection is not None
    assert type(projection.estimated) is date
    assert projection.estimated == date(2026, 11, 1)
    event = projection.calendar_event()
    assert event.start == date(2026, 11, 1)
    assert event.end == date(2026, 11, 2)
    assert type(event.start) is date
    assert "T00:00" not in (event.description or "")
    if not delivered:
        day = DeliveryDay.at(
            datetime.fromisoformat("2026-11-01T19:00:00Z"), "America/Los_Angeles"
        )
        assert projection.due_on(day)


def test_typed_range_retains_inclusive_dates_and_existing_window_priority() -> None:
    record = {
        "id": SHIPMENT_ID,
        "title": "Civil range",
        "status": "in_transit",
        "delivery_date": report(
            "estimated_range", start="2026-10-31", end="2026-11-02"
        ),
    }
    _, projection = delivery_estimate(record)
    assert projection is not None
    event = projection.calendar_event()
    assert event.start == date(2026, 10, 31)
    assert event.end == date(2026, 11, 3)
    assert type(event.start) is date
    _, canonical = delivery_estimate(
        {
            **record,
            "estimated_delivery_window": {
                "start": "2026-11-03",
                "end": "2026-11-04",
            },
        }
    )
    assert canonical is not None
    assert canonical.calendar_event().start == date(2026, 11, 3)


@pytest.mark.parametrize("delivered", [False, True])
async def test_actual_timestamp_or_canonical_eta_wins_over_civil_fallback(
    hass: HomeAssistant, delivered: bool
) -> None:
    await hass.config.async_set_time_zone("America/Los_Angeles")
    field = "delivered_at" if delivered else "estimated_delivery_at"
    _, projection = delivery_estimate(
        {
            "id": SHIPMENT_ID,
            "title": "Higher precision",
            "status": "delivered" if delivered else "in_transit",
            field: "2026-10-31T22:30:00Z",
            "delivery_date": report(
                "delivered" if delivered else "estimated", date="2026-11-01"
            ),
        }
    )
    assert projection is not None
    assert isinstance(projection.estimated, datetime)
    assert projection.calendar_event().start == date(2026, 10, 31)


@pytest.mark.parametrize(
    "value",
    [
        report("estimated", date="2026-02-30"),
        report("estimated", date="2026-10-31T00:00:00Z"),
        report("estimated", date="2026-10-31", timezone="Invalid/Zone"),
        report("estimated", date="2026-10-31", observed_at="2026-10-31"),
        report("estimated_range", start="2026-11-02", end="2026-10-31"),
        report("estimated_range", start="2026-10-31"),
        report("delivered", date="2026-10-31"),
        {"kind": "estimated", "date": "2026-10-31"},
    ],
)
def test_invalid_or_wrong_kind_reports_do_not_invent_active_dates(
    value: object,
) -> None:
    assert (
        delivery_estimate(
            {
                "id": SHIPMENT_ID,
                "title": "Unsupported report",
                "status": "in_transit",
                "delivery_date": value,
            }
        )[1]
        is None
    )


def test_day_item_uses_civil_equality_and_canonical_scan_priority() -> None:
    day = DeliveryDay.at(
        datetime.fromisoformat("2026-10-31T19:00:00Z"), "America/Los_Angeles"
    )
    record = FixtureShipment(status="delivered").record()
    record["carrier_tracking"] = {
        "delivery_date": report("delivered", date="2026-10-31")
    }
    item = ShipmentItem.parse(record, day)
    assert type(item.delivered_at) is date
    assert item.attributes()["delivered_at"] == "2026-10-31"
    assert day.query()["delivered_on"] == "2026-10-31"
    with pytest.raises(ValueError, match="outside the local delivery day"):
        ShipmentItem.parse(
            {
                **record,
                "carrier_tracking": {
                    "delivered_at": "2026-11-01T08:00:00Z",
                    "delivery_date": report("delivered", date="2026-10-31"),
                },
            },
            day,
        )
