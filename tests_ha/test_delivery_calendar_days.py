"""Delivery-day visibility and truthful delivered history across local days."""

import json
from datetime import date, datetime, timedelta
from uuid import UUID
from zoneinfo import ZoneInfo

import pytest
from homeassistant.core import HomeAssistant

from custom_components.shipments.models import DeliveryEstimate, delivery_estimate

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
    assert raw in event.description
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


def window_projection(start: str, end: str, **extra) -> DeliveryEstimate | None:
    """Synthetic backend projection, never a carrier request."""
    _, projection = delivery_estimate(
        {
            "id": SHIPMENT,
            "title": "Synthetic parcel",
            "status": "in_transit",
            "estimated_delivery_window": {"start": start, "end": end},
            **extra,
        }
    )
    return projection


@pytest.mark.parametrize("delivered", [False, True])
def test_legacy_positional_estimates_keep_date_and_delivered_arguments(
    delivered: bool,
) -> None:
    projection = DeliveryEstimate(
        UUID(SHIPMENT), "Synthetic parcel", "UPS", date(2026, 10, 27), delivered
    )
    event = projection.calendar_event()
    assert event.start == date(2026, 10, 27)
    assert event.end == date(2026, 10, 28)
    assert event.summary == (
        "Delivered: Synthetic parcel" if delivered else "Synthetic parcel"
    )


@pytest.mark.parametrize(
    ("start", "end", "hours"),
    [
        ("2026-10-27T12:00:00-07:00", "2026-10-27T14:00:00-07:00", 2),
        ("2026-03-08T01:30:00-08:00", "2026-03-08T03:30:00-07:00", 1),
        ("2026-11-01T01:30:00-07:00", "2026-11-01T01:30:00-08:00", 1),
        ("2026-10-27T23:00:00-07:00", "2026-10-28T01:00:00-07:00", 2),
        ("2026-10-27T00:00:00-07:00", "2026-10-27T02:00:00-07:00", 2),
        ("2026-10-27T12:00:00Z", "2026-10-27T14:00:00+00:00", 2),
    ],
)
async def test_genuine_windows_preserve_instants_offsets_and_raw_evidence(
    hass: HomeAssistant, start: str, end: str, hours: int
) -> None:
    await hass.config.async_set_time_zone("America/Los_Angeles")
    # No point ETA is necessary, and a distinct point must not replace bounds.
    for extra in ({}, {"estimated_delivery_at": "2026-10-27T18:00:00Z"}):
        projection = window_projection(start, end, **extra)
        assert projection is not None
        event = projection.calendar_event()
        assert isinstance(event.start, datetime)
        assert isinstance(event.end, datetime)
        assert event.start == datetime.fromisoformat(start)
        assert event.end == datetime.fromisoformat(end)
        assert event.start.utcoffset() == datetime.fromisoformat(start).utcoffset()
        assert event.end.utcoffset() == datetime.fromisoformat(end).utcoffset()
        assert (event.end.timestamp() - event.start.timestamp()) / 3600 == hours
        assert event.description is not None
        assert f"Carrier window: {start} to {end}." in event.description
        if extra:
            assert extra["estimated_delivery_at"] in event.description


@pytest.mark.parametrize(
    ("start", "end", "point", "first", "last"),
    [
        ("2026-10-27", "2026-10-29", None, "2026-10-27", "2026-10-30"),
        ("2026-10-27", "2026-10-27", None, "2026-10-27", "2026-10-28"),
        ("2026-03-08", "2026-03-08", None, "2026-03-08", "2026-03-09"),
        ("2026-11-01", "2026-11-01", None, "2026-11-01", "2026-11-02"),
        (
            "2026-10-27T00:00:00Z",
            "2026-10-28T00:00:00Z",
            None,
            "2026-10-27",
            "2026-10-29",
        ),
        (
            "2026-10-27T00:00:00-07:00",
            "2026-10-29T00:00:00-07:00",
            "2026-10-28T18:00:00Z",
            "2026-10-28",
            "2026-10-29",
        ),
        (
            "2026-10-27T00:00:00Z",
            "2026-10-28T00:00:00Z",
            "2026-10-30",
            "2026-10-27",
            "2026-10-29",
        ),
    ],
)
async def test_date_precision_windows_never_become_timed_appointments(
    hass: HomeAssistant,
    start: str,
    end: str,
    point: str | None,
    first: str,
    last: str,
) -> None:
    await hass.config.async_set_time_zone("America/Los_Angeles")
    projection = window_projection(start, end, estimated_delivery_at=point)
    assert projection is not None
    event = projection.calendar_event()
    assert type(event.start) is date
    assert type(event.end) is date
    assert event.start == date.fromisoformat(first)
    assert event.end == date.fromisoformat(last)
    assert event.description is not None
    assert start in event.description and end in event.description


@pytest.mark.parametrize(
    "window",
    [
        None,
        {},
        {"start": "2026-10-27"},
        {"end": "2026-10-28"},
        {"start": 123, "end": "2026-10-28"},
        {"start": "2026-10-28", "end": "2026-10-27"},
        {"start": "2026-02-30", "end": "2026-10-27"},
        {"start": "bad", "end": "2026-10-27"},
        {"start": "2026-10-27", "end": "2026-10-27T14:00:00Z"},
        {"start": "2026-10-27T12:00:00", "end": "2026-10-27T14:00:00"},
        {"start": "2026-10-27T14:00:00Z", "end": "2026-10-27T12:00:00Z"},
        {"start": "2026-10-27T12:00:00Z", "end": "2026-10-27T12:00:00Z"},
        {"start": "2026-10-27 12:00:00Z", "end": "2026-10-27 14:00:00Z"},
        {"start": "2026-10-27T12:00:00+25:00", "end": "2026-10-27T14:00:00Z"},
        {"start": "2026-10-27T12:00:00+01:99", "end": "2026-10-27T14:00:00Z"},
        {"start": "2026-10-27T25:00:00Z", "end": "2026-10-28T02:00:00Z"},
        {"start": "2026-02-30T12:00:00Z", "end": "2026-10-28T02:00:00Z"},
    ],
)
def test_invalid_or_unpaired_windows_cannot_invent_dates(window: object) -> None:
    record = {
        "id": SHIPMENT,
        "title": "Synthetic parcel",
        "status": "in_transit",
        "estimated_delivery_window": window,
    }
    assert delivery_estimate(record)[1] is None
    _, projection = delivery_estimate({**record, "estimated_delivery_at": "2026-10-27"})
    assert projection is not None
    event = projection.calendar_event()
    assert event.start == date(2026, 10, 27)
    assert event.end == date(2026, 10, 28)
    assert "Carrier window:" not in (event.description or "")


@pytest.mark.parametrize("actual", [None, "invalid", "2026-10-28T18:30:00Z"])
def test_delivered_windows_never_replace_verified_actual(actual: str | None) -> None:
    projection = window_projection(
        "2026-10-27T12:00:00-07:00",
        "2026-10-27T14:00:00-07:00",
        status="delivered",
        delivered_at=actual,
    )
    if actual is None or actual == "invalid":
        assert projection is None
    else:
        assert projection is not None
        event = projection.calendar_event()
        assert type(event.start) is date
        assert event.start == date(2026, 10, 28)
        assert event.end == date(2026, 10, 29)
        assert event.summary == "Delivered: Synthetic parcel"
        assert "Carrier window:" not in (event.description or "")
        assert actual in (event.description or "")


async def test_ended_today_windows_remain_in_full_day_query(
    hass: HomeAssistant, entry, backend, hass_client, monkeypatch
) -> None:
    from custom_components.shipments.calendar import InboundDeliveryCalendar

    await hass.config.async_set_time_zone("America/Los_Angeles")
    projection = window_projection(
        "2026-10-27T12:00:00-07:00", "2026-10-27T14:00:00-07:00"
    )
    assert projection is not None
    backend.responses[("GET", "/v1/delivery-estimates?limit=200")] = (
        200,
        json.dumps(
            {
                "items": [
                    {
                        "id": SHIPMENT,
                        "title": "Synthetic parcel",
                        "status": "in_transit",
                        "estimated_delivery_window": {
                            "start": "2026-10-27T12:00:00-07:00",
                            "end": "2026-10-27T14:00:00-07:00",
                        },
                    }
                ],
                "next_page": None,
            }
        ).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    assert entry.runtime_data.inbound_estimates == (projection,)
    calendar = InboundDeliveryCalendar(entry.runtime_data, entry)
    now = datetime.fromisoformat("2026-10-27T16:00:00-07:00")
    monkeypatch.setattr("custom_components.shipments.calendar.dt_util.now", lambda: now)
    assert calendar.event is None  # A passed window does not become a current event.
    start = datetime.fromisoformat("2026-10-27T00:00:00-07:00")
    end = datetime.fromisoformat("2026-10-28T00:00:00-07:00")
    events = await calendar.async_get_events(hass, start, end)
    assert len(events) == 1
    assert projection.window is not None
    assert events[0].start == projection.window.start
    assert events[0].end == projection.window.end
    assert await calendar.async_get_events(hass, now, end) == []
    # Intersections are half-open, preserving the authentic endpoint.
    assert isinstance(events[0].end, datetime)
    assert await calendar.async_get_events(hass, events[0].end, end) == []
    client = await hass_client()
    response = await client.get(
        "/api/calendars/calendar.shipments_inbound_deliveries",
        params={"start": start.isoformat(), "end": end.isoformat()},
    )
    assert response.status == 200
    serialized = await response.json()
    assert len(serialized) == 1
    assert serialized[0]["start"] == {"dateTime": "2026-10-27T12:00:00-07:00"}
    assert serialized[0]["end"] == {"dateTime": "2026-10-27T14:00:00-07:00"}
