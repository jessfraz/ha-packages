"""Assist queries use the loaded adapter and a real, bounded HTTP backend."""

import asyncio
import json
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from typing import TypedDict, Unpack
from uuid import UUID

import pytest
from aiohttp import web
from homeassistant.components.homeassistant import exposed_entities
from homeassistant.core import Context, HomeAssistant
from homeassistant.helpers import entity_registry as er
from homeassistant.loader import async_get_integration
from homeassistant.setup import async_setup_component
from pytest_homeassistant_custom_component.common import MockConfigEntry, MockUser

from custom_components.shipments.const import CONF_BACKEND_URL, CONF_TOKEN, DOMAIN
from custom_components.shipments.models import Direction
from custom_components.shipments.voice import (
    ShipmentDay,
    ShipmentQuery,
    VoiceAnswer,
    VoiceRequest,
    async_query,
    query_enabled,
)

from .conftest import SHIPMENT_ID, Backend, FixtureShipment

GATEWAY = "sensor.incoming_packages"


@dataclass(frozen=True)
class VoiceShipment(FixtureShipment):
    error: str | None = None

    def record(self) -> dict[str, object]:
        record = super().record()
        record["sync"] = {
            "last_error": self.error,
            "tracker_id": "PRIVATE-PROVIDER-TRACKER",
        }
        return record


@dataclass
class VoiceBackend(Backend):
    query_started: asyncio.Event = field(default_factory=asyncio.Event)
    release_query: asyncio.Event = field(default_factory=asyncio.Event)
    hold_query: bool = False
    fail_query: bool = False

    def selected_shipments(self, query: dict[str, str]) -> list[FixtureShipment]:
        rows = super().selected_shipments(query)
        if query.get("active") == "true":
            rows = [
                row
                for row in rows
                if row.status not in {"delivered", "return_to_sender", "cancelled"}
            ]
        if "search" in query:
            rows = [
                row
                for row in rows
                if query["search"].casefold() in row.title.casefold()
            ]
        if query.get("tracking_issues") == "true":
            rows[:] = [
                row for row in rows if isinstance(row, VoiceShipment) and row.error
            ]
        if query.get("exceptions") == "true":
            rows = [
                row
                for row in rows
                if row.status in {"failure", "error", "return_to_sender"}
            ]
        return rows

    async def handle(self, request: web.Request) -> web.Response:
        if request.path == "/v1/shipments" and request.query.get("limit") == "200":
            self.query_started.set()
            if self.hold_query:
                await self.release_query.wait()
            if self.fail_query:
                return web.json_response({"secret": "PRIVATE-ERROR-BODY"}, status=503)
        return await super().handle(request)


class ShipmentFields(TypedDict, total=False):
    title: str
    status: str
    direction: str
    carrier: str
    delivered_at: str | None
    estimated_delivery: str | None
    archived: bool
    error: str | None


def shipment(index: int = 0, **fields: Unpack[ShipmentFields]) -> VoiceShipment:
    return VoiceShipment(id=str(UUID(int=UUID(SHIPMENT_ID).int + index)), **fields)


@pytest.fixture
async def voice_backend(
    hass, aiohttp_server, socket_enabled
) -> AsyncIterator[VoiceBackend]:
    backend = VoiceBackend()
    backend.version = str((await async_get_integration(hass, DOMAIN)).version)
    app = web.Application()
    app.router.add_route("*", "/{path:.*}", backend.handle)
    server = await aiohttp_server(app, access_log=None)
    backend.url = str(server.make_url("")).rstrip("/")
    yield backend


@pytest.fixture
async def voice_entry(
    hass: HomeAssistant, voice_backend: VoiceBackend, freezer
) -> MockConfigEntry:
    freezer.move_to("2026-10-15T17:00:00Z")
    await hass.config.async_set_time_zone("America/Los_Angeles")
    assert await async_setup_component(hass, "homeassistant", {})
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={CONF_BACKEND_URL: voice_backend.url, CONF_TOKEN: "test-token"},
    )
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    registry = er.async_get(hass)
    gateway = registry.async_get_entity_id(
        "sensor", DOMAIN, f"{entry.entry_id}_inbound_active"
    )
    assert gateway is not None
    registry.async_update_entity(gateway, new_entity_id=GATEWAY)
    await hass.async_block_till_done(wait_background_tasks=True)
    exposed_entities.async_expose_entity(hass, "conversation", GATEWAY, True)
    voice_backend.requests.clear()
    return entry


async def ask(
    hass: HomeAssistant,
    query: ShipmentQuery,
    *,
    direction: Direction | None = Direction.INBOUND,
    day: ShipmentDay = ShipmentDay.TODAY,
    name: str | None = None,
    context: Context | None = None,
    assistant: str | None = None,
) -> VoiceAnswer:
    return await async_query(
        hass, VoiceRequest(query, direction, day, name), context or Context(), assistant
    )


@pytest.mark.parametrize("query", list(ShipmentQuery))
async def test_empty_is_a_real_zero_not_a_missing_projection(
    hass: HomeAssistant, voice_entry, voice_backend: VoiceBackend, query: ShipmentQuery
) -> None:
    voice_backend.shipment_rows = []
    answer = await ask(hass, query, name="Office supplies")
    assert answer.error is None
    assert answer.count == 0
    assert answer.as_dict()["items"] == []
    assert answer.as_dict()["truncated"] is False
    assert answer.count_exact is True
    assert all(request.method == "GET" for request in voice_backend.requests)
    assert all(request.path == "/v1/shipments" for request in voice_backend.requests)
    assert all(request.query["limit"] == "200" for request in voice_backend.requests)


async def test_arriving_uses_local_eta_and_includes_out_for_delivery_without_estimate(
    hass: HomeAssistant, voice_entry, voice_backend: VoiceBackend
) -> None:
    voice_backend.shipment_rows = [
        shipment(
            title="  Original CASE 🚚  ",
            status="in_transit",
            estimated_delivery="2026-10-15",
        ),
        shipment(1, title="Carrier scan", carrier="FedEx"),
        shipment(
            2,
            title="Tonight UTC",
            status="in_transit",
            estimated_delivery="2026-10-16T06:59:59Z",
        ),
        shipment(
            3,
            title="Tomorrow local",
            status="in_transit",
            estimated_delivery="2026-10-16T07:00:00Z",
        ),
        shipment(4, title="Outbound", direction="outbound"),
        shipment(
            5,
            title="Already delivered",
            status="delivered",
            estimated_delivery="2026-10-15",
        ),
        shipment(6, title="Archived", archived=True),
    ]
    answer = await ask(hass, ShipmentQuery.ARRIVING)
    assert answer.count == 3
    assert {item["title"] for item in answer.as_dict()["items"]} == {
        "  Original CASE 🚚  ",
        "Carrier scan",
        "Tonight UTC",
    }
    assert (
        "Carrier scan via FedEx, out for delivery, delivery estimate unavailable"
        in answer.speech
    )
    assert "Original CASE" in answer.speech
    assert voice_backend.requests[0].query == {
        "archived": "false",
        "limit": "200",
        "sort": "expected",
        "direction": "inbound",
        "active": "true",
    }
    tomorrow = await ask(hass, ShipmentQuery.ARRIVING, day=ShipmentDay.TOMORROW)
    assert tomorrow.count == 1
    assert tomorrow.as_dict()["items"][0]["title"] == "Tomorrow local"


@pytest.mark.parametrize(
    "direction,count", [(Direction.INBOUND, 1), (Direction.OUTBOUND, 1), (None, 2)]
)
async def test_explicit_direction_and_all(
    hass: HomeAssistant,
    voice_entry,
    voice_backend: VoiceBackend,
    direction: Direction | None,
    count: int,
) -> None:
    voice_backend.shipment_rows = [shipment(), shipment(1, direction="outbound")]
    answer = await ask(hass, ShipmentQuery.OUT_FOR_DELIVERY, direction=direction)
    assert answer.count == count
    if direction:
        assert {item["direction"] for item in answer.as_dict()["items"]} == {
            direction.value
        }
    else:
        assert "direction" not in voice_backend.requests[-1].query


async def test_out_for_delivery_is_sorted_by_eta_and_preserves_safe_links(
    hass: HomeAssistant, voice_entry, voice_backend: VoiceBackend
) -> None:
    voice_backend.shipment_rows = [
        shipment(title="Z early", estimated_delivery="2026-10-15T17:00:00Z"),
        shipment(1, title="A later", estimated_delivery="2026-10-15T20:00:00Z"),
        shipment(2, title="Unknown ETA"),
    ]
    answer = await ask(hass, ShipmentQuery.OUT_FOR_DELIVERY)
    items = answer.as_dict()["items"]
    assert [item["title"] for item in items] == ["Z early", "A later", "Unknown ETA"]
    assert items[0]["url"] == f"/shipments/shipments/shipment%3D{SHIPMENT_ID}"
    assert items[0]["estimated_delivery"] == "2026-10-15T17:00:00+00:00"
    serialized = json.dumps(answer.as_dict()) + answer.speech + answer.card_text
    for private in (
        "PRIVATE-TRACKING-NUMBER",
        "PRIVATE-PROVIDER-TRACKER",
        "PRIVATE-ORDER",
        "PRIVATE-SOURCE",
        "Private recipient",
    ):
        assert private not in serialized


@pytest.mark.parametrize(
    "day_start,hours", [("2026-03-08T08:00:00Z", 23), ("2026-11-01T07:00:00Z", 25)]
)
async def test_delivered_today_uses_actual_carrier_scan_and_dst_boundaries(
    hass: HomeAssistant,
    voice_entry,
    voice_backend: VoiceBackend,
    freezer,
    day_start: str,
    hours: int,
) -> None:
    start = datetime.fromisoformat(day_start)
    end = start + timedelta(hours=hours)
    freezer.move_to(start + timedelta(hours=12))
    voice_backend.shipment_rows = [
        shipment(
            title="At midnight", status="delivered", delivered_at=start.isoformat()
        ),
        shipment(
            1,
            title="Latest",
            status="failure",
            delivered_at=(end - timedelta(seconds=1)).isoformat(),
        ),
        shipment(
            2,
            title="Previous day",
            status="delivered",
            delivered_at=(start - timedelta(seconds=1)).isoformat(),
        ),
        shipment(3, title="Next day", status="delivered", delivered_at=end.isoformat()),
        shipment(4, title="Unknown scan", status="delivered"),
        shipment(
            5,
            title="Outbound",
            direction="outbound",
            status="delivered",
            delivered_at=start.isoformat(),
        ),
    ]
    answer = await ask(hass, ShipmentQuery.DELIVERED)
    assert answer.count == 2
    assert [item["title"] for item in answer.as_dict()["items"]] == [
        "Latest",
        "At midnight",
    ]
    request = next(
        request
        for request in voice_backend.requests
        if request.path == "/v1/shipments" and "delivered_after" in request.query
    )
    assert request.query["delivered_after"] == str(int(start.timestamp()))
    assert request.query["delivered_before"] == str(int(end.timestamp()))


async def test_status_title_lookup_needs_unambiguous_match(
    hass: HomeAssistant, voice_entry, voice_backend: VoiceBackend
) -> None:
    voice_backend.shipment_rows = [
        shipment(
            title="Office supplies",
            status="in_transit",
            estimated_delivery="2026-10-16",
        ),
        shipment(1, title="Office furniture"),
    ]
    ambiguous = await ask(hass, ShipmentQuery.STATUS, name="Office")
    assert ambiguous.error == "ambiguous_name"
    assert ambiguous.as_dict()["items"] == []
    precise = await ask(hass, ShipmentQuery.STATUS, name="office SUPPLIES")
    assert precise.count == 1
    assert precise.error is None
    assert (
        "Office supplies via USPS, in transit, estimated October 16" in precise.speech
    )
    missing_name = await ask(hass, ShipmentQuery.STATUS)
    assert missing_name.error == "name_required"


async def test_status_lookup_accepts_a_complete_valid_500_character_title(
    hass: HomeAssistant, voice_entry, voice_backend: VoiceBackend
) -> None:
    title = "Original shipment title " + "x" * (500 - len("Original shipment title "))
    voice_backend.shipment_rows = [shipment(title=title, status="in_transit")]
    answer = await ask(hass, ShipmentQuery.STATUS, name=title)
    assert answer.error is None
    assert answer.count == 1
    assert answer.as_dict()["items"][0]["title"] == title
    assert title in answer.speech
    assert voice_backend.requests[-1].query["search"] == title


@pytest.mark.parametrize(
    "name,error",
    [("", "name_required"), ("   ", "name_required"), ("x" * 501, "invalid_name")],
)
async def test_status_lookup_rejects_blank_and_overlong_titles_without_backend_reads(
    hass: HomeAssistant, voice_entry, voice_backend: VoiceBackend, name: str, error: str
) -> None:
    answer = await ask(hass, ShipmentQuery.STATUS, name=name)
    assert answer.error == error
    assert answer.count is None
    assert not voice_backend.requests


async def test_issues_merge_exception_and_tracking_error_without_private_messages(
    hass: HomeAssistant, voice_entry, voice_backend: VoiceBackend
) -> None:
    voice_backend.shipment_rows = [
        shipment(title="Carrier exception", status="failure", error="PRIVATE-ERROR"),
        shipment(1, title="Only tracking", error="PRIVATE-ERROR"),
        shipment(2, title="Only exception", status="return_to_sender"),
        shipment(3, title="Healthy"),
        shipment(4, title="Outbound", direction="outbound", error="PRIVATE-ERROR"),
    ]
    answer = await ask(hass, ShipmentQuery.ISSUES)
    assert answer.count == 3
    assert len(voice_backend.requests) == 2
    assert {"tracking_issues", "exceptions"} == {
        next(key for key in request.query if key in {"tracking_issues", "exceptions"})
        for request in voice_backend.requests
    }
    assert {item["title"] for item in answer.as_dict()["items"]} == {
        "Carrier exception",
        "Only tracking",
        "Only exception",
    }
    serialized = json.dumps(answer.as_dict()) + answer.speech + answer.card_text
    assert "PRIVATE" not in serialized
    assert "Private merchant" not in serialized


async def test_spoken_cap_and_backend_cursor_are_honest(
    hass: HomeAssistant, voice_entry, voice_backend: VoiceBackend
) -> None:
    voice_backend.shipment_rows = [
        shipment(index, title=f"Parcel {index}") for index in range(8)
    ]
    answer = await ask(hass, ShipmentQuery.OUT_FOR_DELIVERY)
    assert answer.count == 8
    assert len(answer.as_dict()["items"]) == 5
    assert answer.count_exact is True
    assert answer.as_dict()["truncated"] is True
    assert "And 3 more" in answer.speech
    voice_backend.shipment_rows = [
        shipment(index, title=f"Parcel {index}") for index in range(201)
    ]
    partial = await ask(hass, ShipmentQuery.OUT_FOR_DELIVERY)
    assert partial.count == 200
    assert partial.count_exact is False
    assert "At least 200" in partial.speech
    assert "More shipments may be available" in partial.speech
    assert len(voice_backend.requests) == 2
    # An ETA filter may find no matches in its bounded first page. That is not zero.
    for_missing_eta = await ask(hass, ShipmentQuery.ARRIVING, day=ShipmentDay.TOMORROW)
    assert for_missing_eta.count == 0
    assert for_missing_eta.count_exact is False
    assert "results are incomplete" in for_missing_eta.speech


async def test_backend_error_never_becomes_zero_or_leaks_response(
    hass: HomeAssistant, voice_entry, voice_backend: VoiceBackend
) -> None:
    voice_backend.fail_query = True
    answer = await ask(hass, ShipmentQuery.OUT_FOR_DELIVERY)
    assert answer.error == "backend_unavailable"
    assert answer.count is None
    assert "PRIVATE" not in json.dumps(answer.as_dict())


@pytest.mark.parametrize(
    "body",
    [
        b"not-json PRIVATE",
        b'{"items":[{"title":"PRIVATE"}],"next_page":null}',
        b'{"items":[],"next_page":42}',
    ],
)
async def test_malformed_page_never_becomes_false_zero(
    hass: HomeAssistant, voice_entry, voice_backend: VoiceBackend, body: bytes
) -> None:
    voice_backend.responses[("GET", "/v1/shipments")] = (200, body, {})
    answer = await ask(hass, ShipmentQuery.OUT_FOR_DELIVERY)
    assert answer.error == "backend_unavailable"
    assert answer.count is None
    assert "PRIVATE" not in json.dumps(answer.as_dict())


async def test_gateway_exposure_and_user_read_permission(
    hass: HomeAssistant, voice_entry, voice_backend: VoiceBackend, hass_user: MockUser
) -> None:
    assert query_enabled(hass)
    exposed_entities.async_expose_entity(hass, "conversation", GATEWAY, False)
    assert not query_enabled(hass)
    hidden = await ask(hass, ShipmentQuery.ARRIVING)
    assert hidden.error == "not_exposed"
    assert not voice_backend.requests
    exposed_entities.async_expose_entity(hass, "conversation", GATEWAY, True)
    hass_user.mock_policy({"entities": {"all": {"read": False}}})
    denied = await ask(
        hass, ShipmentQuery.ARRIVING, context=Context(user_id=hass_user.id)
    )
    assert denied.error == "unauthorized"
    assert not voice_backend.requests
    hass_user.mock_policy({"entities": {"entity_ids": {GATEWAY: {"read": True}}}})
    allowed = await ask(
        hass, ShipmentQuery.ARRIVING, context=Context(user_id=hass_user.id)
    )
    assert allowed.error is None
    hass_user.is_active = False
    inactive = await ask(
        hass, ShipmentQuery.ARRIVING, context=Context(user_id=hass_user.id)
    )
    assert inactive.error == "unauthorized"
    unknown = await ask(
        hass, ShipmentQuery.ARRIVING, context=Context(user_id="missing")
    )
    assert unknown.error == "unauthorized"
    another_assistant = await ask(
        hass, ShipmentQuery.ARRIVING, assistant="google_assistant"
    )
    assert another_assistant.error == "not_exposed"


async def test_unhealthy_mixed_version_and_unloaded_never_answer_from_stale_data(
    hass: HomeAssistant, voice_entry, voice_backend: VoiceBackend
) -> None:
    voice_backend.responses[("GET", "/v1/system")] = (503, b"unavailable", {})
    await voice_entry.runtime_data.async_refresh()
    failed = await ask(hass, ShipmentQuery.ARRIVING)
    assert failed.error == "unavailable"
    voice_backend.responses.clear()
    voice_backend.version = "0.0.0"
    await voice_entry.runtime_data.async_refresh()
    mismatch = await ask(hass, ShipmentQuery.DELIVERED)
    assert mismatch.error == "unavailable"
    assert await hass.config_entries.async_unload(voice_entry.entry_id)
    unloaded = await ask(hass, ShipmentQuery.ARRIVING)
    assert unloaded.error == "unavailable"
    assert not query_enabled(hass)


async def test_exposure_revoked_while_query_in_flight_discards_answer(
    hass: HomeAssistant, voice_entry, voice_backend: VoiceBackend
) -> None:
    voice_backend.shipment_rows = [shipment(title="Should not be disclosed")]
    voice_backend.hold_query = True
    task = hass.async_create_task(ask(hass, ShipmentQuery.OUT_FOR_DELIVERY))
    await voice_backend.query_started.wait()
    exposed_entities.async_expose_entity(hass, "conversation", GATEWAY, False)
    voice_backend.release_query.set()
    answer = await task
    assert answer.error == "not_exposed"
    assert "Should not be disclosed" not in json.dumps(answer.as_dict())
