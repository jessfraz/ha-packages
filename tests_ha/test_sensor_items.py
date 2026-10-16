"""Safe, bounded package details flow through the real poll and native sensors."""

import json
from dataclasses import replace
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import TypedDict, Unpack
from urllib.parse import urlencode
from uuid import UUID

import pytest
from homeassistant.components.recorder import Recorder, get_instance
from homeassistant.components.recorder.history import get_last_state_changes
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import ConfigEntryAuthFailed
from homeassistant.helpers.recorder import async_initialize_recorder
from homeassistant.setup import async_setup_component

from .conftest import SHIPMENT_ID, Backend, FixtureShipment, ReceivedRequest, get_state

TODAY_ENTITY = "sensor.shipments_inbound_delivered_today"
ARRIVING_ENTITY = "sensor.shipments_inbound_out_for_delivery"
DAY_START = datetime(2026, 10, 15, 7, tzinfo=UTC)
DAY_END = DAY_START + timedelta(days=1)
ITEM_KEYS = {
    "id",
    "title",
    "carrier",
    "status",
    "delivered_at",
    "estimated_delivery",
    "url",
}


class ShipmentFields(TypedDict, total=False):
    title: str
    carrier: str
    status: str
    direction: str
    delivered_at: str | None
    estimated_delivery: str | None
    archived: bool


def shipment(index: int = 0, **fields: Unpack[ShipmentFields]) -> FixtureShipment:
    return FixtureShipment(id=str(UUID(int=UUID(SHIPMENT_ID).int + index)), **fields)


@pytest.fixture
async def recording(hass: HomeAssistant, tmp_path: Path) -> Recorder:
    """Start native recording before the integration installs its entities."""
    async_initialize_recorder(hass)
    assert await async_setup_component(
        hass,
        "recorder",
        {
            "recorder": {
                "db_url": f"sqlite:///{tmp_path / 'history.db'}",
                "commit_interval": 0,
            }
        },
    )
    return get_instance(hass)


async def refresh_day(hass: HomeAssistant, entry, freezer, backend: Backend) -> None:
    freezer.move_to(DAY_START + timedelta(hours=10))
    await hass.config.async_set_time_zone("America/Los_Angeles")
    # Drain config-change work and the setup check-in before measuring one poll.
    await hass.async_block_till_done(wait_background_tasks=True)
    backend.requests.clear()
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done(wait_background_tasks=True)


def shipment_requests(backend: Backend) -> list[ReceivedRequest]:
    return [request for request in backend.requests if request.path == "/v1/shipments"]


async def test_empty_item_lists_are_present_and_use_two_bounded_requests(
    hass: HomeAssistant, entry, backend: Backend, freezer
) -> None:
    backend.shipment_rows = []
    backend.requests.clear()
    await refresh_day(hass, entry, freezer, backend)
    for entity_id in (TODAY_ENTITY, ARRIVING_ENTITY):
        state = get_state(hass, entity_id)
        assert state.state == "0"
        assert state.attributes.get("items") == [], "Count sensor lacks package items"
        assert state.attributes["items_truncated"] is False
    lists = shipment_requests(backend)
    assert len(lists) == 2
    assert {tuple(sorted(request.query.items())) for request in lists} == {
        tuple(
            sorted(
                {
                    "direction": "inbound",
                    "archived": "false",
                    "delivered_after": str(int(DAY_START.timestamp())),
                    "delivered_before": str(int(DAY_END.timestamp())),
                    "sort": "delivered",
                    "limit": "20",
                }.items()
            )
        ),
        tuple(
            sorted(
                {
                    "direction": "inbound",
                    "archived": "false",
                    "status": "out_for_delivery",
                    "sort": "expected",
                    "limit": "20",
                }.items()
            )
        ),
    }
    summary = next(
        request
        for request in backend.requests
        if request.path == "/v1/summary" and "delivered_after" in request.query
    )
    delivered = next(request for request in lists if "delivered_after" in request.query)
    assert all(delivered.query[key] == value for key, value in summary.query.items())
    assert not any("page_token" in request.query for request in lists)


async def test_one_item_preserves_title_and_exposes_only_safe_deep_link_fields(
    hass: HomeAssistant, entry, backend: Backend, freezer
) -> None:
    title = "  New view / original CASE 🚚  "
    delivered = shipment(
        title=title,
        status="delivered",
        delivered_at="2026-10-15T10:00:00Z",
        estimated_delivery="2026-10-15",
    )
    arriving = shipment(1, title=title, carrier="FedEx")
    backend.shipment_rows = [delivered, arriving]
    await refresh_day(hass, entry, freezer, backend)
    for entity_id, row in ((TODAY_ENTITY, delivered), (ARRIVING_ENTITY, arriving)):
        state = get_state(hass, entity_id)
        assert state.state == "1"
        assert state.attributes["items_truncated"] is False
        items = state.attributes["items"]
        assert len(items) == int(state.state)
        assert set(items[0]) == ITEM_KEYS
        assert items[0] == {
            "id": row.id,
            "title": title,
            "carrier": row.carrier,
            "status": row.status,
            "delivered_at": datetime.fromisoformat(row.delivered_at).isoformat()
            if row.delivered_at
            else None,
            "estimated_delivery": row.estimated_delivery,
            "url": f"/shipments/shipments/shipment%3D{row.id}",
        }
        attributes = json.dumps(dict(state.attributes))
        for private in (
            row.tracking_number,
            "PRIVATE-ORDER",
            "PRIVATE-SOURCE",
            "PRIVATE-PROVIDER-TRACKER",
            "Private recipient",
        ):
            assert private not in attributes
    assert "items" not in get_state(hass, "sensor.shipments_active").attributes


async def test_items_exclude_outbound_archived_and_other_days_and_keep_sort_order(
    hass: HomeAssistant, entry, backend: Backend, freezer
) -> None:
    at_start = shipment(
        status="delivered",
        delivered_at=DAY_START.isoformat(),
        title="At local midnight",
    )
    latest = shipment(
        1,
        status="failure",
        delivered_at="2026-10-15T23:00:00Z",
        title="Actual delivery survives a later status correction",
    )
    later_arrival = shipment(2, estimated_delivery="2026-10-16")
    first_arrival = shipment(3, estimated_delivery="2026-10-15T19:00:00Z")
    unknown_arrival = shipment(4)
    backend.shipment_rows = [
        at_start,
        shipment(5, status="delivered", delivered_at="2026-10-15T06:59:59.999999Z"),
        shipment(6, status="delivered", delivered_at=DAY_END.isoformat()),
        shipment(
            7,
            direction="outbound",
            status="delivered",
            delivered_at="2026-10-15T12:00:00Z",
        ),
        shipment(8, status="delivered"),
        shipment(
            9,
            archived=True,
            status="delivered",
            delivered_at="2026-10-15T12:00:00Z",
        ),
        latest,
        later_arrival,
        shipment(10, direction="outbound", estimated_delivery="2026-10-15"),
        first_arrival,
        unknown_arrival,
        shipment(11, archived=True),
    ]
    await refresh_day(hass, entry, freezer, backend)
    today = get_state(hass, TODAY_ENTITY)
    assert today.state == "2"
    assert [item["id"] for item in today.attributes["items"]] == [
        latest.id,
        at_start.id,
    ]
    arriving = get_state(hass, ARRIVING_ENTITY)
    assert arriving.state == "3"
    assert [item["id"] for item in arriving.attributes["items"]] == [
        first_arrival.id,
        later_arrival.id,
        unknown_arrival.id,
    ]
    assert len(today.attributes["items"]) == int(today.state)
    assert len(arriving.attributes["items"]) == int(arriving.state)


@pytest.mark.parametrize("count", [20, 21, 43])
@pytest.mark.parametrize("delivered", [False, True], ids=["arriving", "delivered"])
async def test_sensor_details_are_capped_without_losing_the_authoritative_count(
    hass: HomeAssistant, entry, backend: Backend, freezer, count: int, delivered: bool
) -> None:
    backend.shipment_rows = [
        shipment(
            index,
            status="delivered" if delivered else "out_for_delivery",
            delivered_at=(DAY_START + timedelta(minutes=index)).isoformat()
            if delivered
            else None,
            estimated_delivery=(DAY_START + timedelta(minutes=index)).isoformat(),
        )
        for index in range(count)
    ]
    backend.requests.clear()
    await refresh_day(hass, entry, freezer, backend)
    state = get_state(hass, TODAY_ENTITY if delivered else ARRIVING_ENTITY)
    assert state.state == str(count)
    items = state.attributes["items"]
    assert len(items) == min(count, 20)
    assert state.attributes["items_truncated"] is (count > 20)
    rows = backend.shipment_rows
    ordered = list(reversed(rows)) if delivered else rows
    assert [item["id"] for item in items] == [row.id for row in ordered[:20]]
    assert len(shipment_requests(backend)) == 2
    assert all(
        "page_token" not in request.query for request in shipment_requests(backend)
    )


@pytest.mark.parametrize(
    "failure",
    [
        "http",
        "auth",
        "missing",
        "extra",
        "outbound",
        "archived",
        "status",
        "duplicate",
        "unexpected_cursor",
        "invalid_cursor",
    ],
)
async def test_bad_arriving_page_never_publishes_a_partial_snapshot(
    hass: HomeAssistant, entry, backend: Backend, failure: str
) -> None:
    previous = entry.runtime_data.data.copy()
    previous_items = entry.runtime_data.shipment_items.copy()
    query = next(
        request.query
        for request in shipment_requests(backend)
        if request.query.get("status") == "out_for_delivery"
    )
    row = shipment()
    rows = [row.record()]
    cursor = None
    status = 200
    if failure == "http":
        status = 503
    elif failure == "auth":
        status = 401
    elif failure == "missing":
        rows = []
    elif failure == "extra":
        rows.append(shipment(1).record())
    elif failure == "outbound":
        rows = [replace(row, direction="outbound").record()]
    elif failure == "archived":
        rows = [replace(row, archived=True).record()]
    elif failure == "status":
        rows = [replace(row, status="in_transit").record()]
    elif failure == "duplicate":
        backend.summaries["inbound"]["out_for_delivery"] = 2
        rows *= 2
    elif failure == "unexpected_cursor":
        cursor = "not-actually-truncated"
    else:
        cursor = 12
    path = f"/v1/shipments?{urlencode(query)}"
    backend.responses[("GET", path)] = (
        status,
        json.dumps({"items": rows, "next_page": cursor}).encode(),
        {},
    )
    backend.summaries[""]["active"] = 100
    await entry.runtime_data.async_refresh()
    assert not entry.runtime_data.last_update_success
    assert entry.runtime_data.data == previous
    assert get_state(hass, ARRIVING_ENTITY).state == "unavailable"
    assert entry.runtime_data.shipment_items == previous_items
    assert not get_state(hass, ARRIVING_ENTITY).attributes.get("items")
    assert get_state(hass, "sensor.shipments_active").state == "unavailable"
    if failure == "auth":
        assert isinstance(entry.runtime_data.last_exception, ConfigEntryAuthFailed)
    del backend.responses[("GET", path)]
    await entry.runtime_data.async_refresh()
    assert entry.runtime_data.last_update_success
    assert get_state(hass, "sensor.shipments_active").state == "100"


@pytest.mark.parametrize(
    "delivered_at", [None, "2026-10-15T06:59:59Z", "2026-10-16T07:00:00Z"]
)
async def test_delivered_page_rejects_rows_outside_the_same_local_day(
    hass: HomeAssistant, entry, backend: Backend, freezer, delivered_at: str | None
) -> None:
    await refresh_day(hass, entry, freezer, backend)
    previous = entry.runtime_data.data.copy()
    request = next(
        request
        for request in shipment_requests(backend)
        if request.query.get("delivered_after") == str(int(DAY_START.timestamp()))
    )
    backend.responses[("GET", f"/v1/shipments?{urlencode(request.query)}")] = (
        200,
        json.dumps(
            {
                "items": [
                    shipment(status="delivered", delivered_at=delivered_at).record()
                ],
                "next_page": None,
            }
        ).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    assert not entry.runtime_data.last_update_success
    assert entry.runtime_data.data == previous
    assert get_state(hass, TODAY_ENTITY).state == "unavailable"


async def test_missing_cursor_does_not_claim_the_truncated_snapshot_is_complete(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    previous = entry.runtime_data.data.copy()
    backend.summaries["inbound"]["out_for_delivery"] = 21
    request = next(
        request
        for request in shipment_requests(backend)
        if request.query.get("status") == "out_for_delivery"
    )
    backend.responses[("GET", f"/v1/shipments?{urlencode(request.query)}")] = (
        200,
        json.dumps(
            {
                "items": [shipment(index).record() for index in range(20)],
                "next_page": None,
            }
        ).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    assert not entry.runtime_data.last_update_success
    assert entry.runtime_data.data == previous
    assert get_state(hass, ARRIVING_ENTITY).state == "unavailable"


async def test_mixed_version_keeps_arriving_details_but_does_not_request_today_items(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    backend.version = "0.5.1"
    backend.requests.clear()
    await entry.runtime_data.async_refresh()
    assert entry.runtime_data.last_update_success
    assert get_state(hass, TODAY_ENTITY).state == "unavailable"
    arriving = get_state(hass, ARRIVING_ENTITY)
    assert arriving.state == "1"
    assert len(arriving.attributes["items"]) == 1
    assert len(shipment_requests(backend)) == 1
    assert not any("delivered_after" in request.query for request in backend.requests)


async def test_package_details_stay_available_live_but_are_excluded_from_recorder(
    recording: Recorder, hass: HomeAssistant, entry, backend: Backend
) -> None:
    backend.summaries["inbound"]["out_for_delivery"] = 2
    backend.delivery_summary["delivered"] = 2
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    await recording.async_block_till_done()
    for entity_id in (TODAY_ENTITY, ARRIVING_ENTITY):
        live = get_state(hass, entity_id)
        assert live.state == "2"
        assert len(live.attributes["items"]) == 2
        history = await recording.async_add_executor_job(
            get_last_state_changes, hass, 1, entity_id
        )
        recorded = history[entity_id][0]
        assert recorded.state == live.state
        assert "items" not in recorded.attributes
        assert "items_truncated" not in recorded.attributes
