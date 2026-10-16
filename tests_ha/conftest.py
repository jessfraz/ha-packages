"""Real Home Assistant and HTTP-server fixtures for the thin adapter."""

import asyncio
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from uuid import UUID

import pytest
from aiohttp import web
from homeassistant.auth.const import GROUP_ID_USER
from homeassistant.core import HomeAssistant, State
from homeassistant.loader import async_get_integration
from pytest_homeassistant_custom_component.common import MockConfigEntry, MockUser

from custom_components.shipments.const import CONF_BACKEND_URL, CONF_TOKEN, DOMAIN

SUMMARY = {
    "active": 4,
    "expected": 1,
    "in_transit": 2,
    "out_for_delivery": 1,
    "delivered": 3,
    "exceptions": 0,
    "sync_errors": 1,
}
SHIPMENT_ID = "0193c69f-e5c4-7e93-b6cb-8172dacb617e"


def get_state(hass: HomeAssistant, entity_id: str) -> State:
    """Read a native entity and fail clearly if the integration did not create it."""
    state = hass.states.get(entity_id)
    assert state is not None, f"Missing Home Assistant entity: {entity_id}"
    return state


@dataclass(frozen=True)
class FixtureShipment:
    """A complete upstream row, including fields the sensors must discard."""

    id: str = SHIPMENT_ID
    title: str = "Office supplies"
    carrier: str = "USPS"
    status: str = "out_for_delivery"
    direction: str = "inbound"
    delivered_at: str | None = None
    estimated_delivery: str | None = None
    archived: bool = False
    tracking_number: str = "PRIVATE-TRACKING-NUMBER"

    def record(self) -> dict[str, object]:
        return {
            "id": self.id,
            "revision": 1,
            "title": self.title,
            "direction": self.direction,
            "merchant": "Private merchant",
            "order_reference": "PRIVATE-ORDER",
            "signature_required": None,
            "tracking": {"number": self.tracking_number, "carrier": self.carrier},
            "status": self.status,
            "carrier_status": self.status,
            "carrier_tracking": {
                "events": [],
                "signed_by": "Private recipient",
                "estimated_delivery_at": self.estimated_delivery,
                "delivered_at": self.delivered_at,
                "events_truncated": False,
            },
            "status_override": None,
            "archived": self.archived,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "sources": [{"provider": "private", "key": "PRIVATE-SOURCE"}],
            "sync": {
                "provider": "easypost",
                "tracker_id": "PRIVATE-PROVIDER-TRACKER",
                "last_error": None,
            },
        }


@dataclass
class ReceivedRequest:
    """A request actually received by the upstream HTTP server."""

    method: str
    path: str
    query: dict[str, str]
    headers: dict[str, str]
    body: bytes


@dataclass
class Backend:
    """A controllable upstream server without replacing adapter transport."""

    url: str = ""
    requests: list[ReceivedRequest] = field(default_factory=list)
    responses: dict[tuple[str, str], tuple[int, bytes, dict[str, str]]] = field(
        default_factory=dict
    )
    summaries: dict[str, dict[str, int]] = field(
        default_factory=lambda: {
            "": SUMMARY.copy(),
            "inbound": {
                **SUMMARY,
                "active": 3,
                "in_transit": 1,
                "delivered": 2,
                "sync_errors": 0,
            },
            "outbound": {
                **SUMMARY,
                "active": 1,
                "expected": 0,
                "in_transit": 1,
                "out_for_delivery": 0,
                "delivered": 1,
            },
        }
    )
    delivery_summary: dict[str, int] = field(
        default_factory=lambda: {**SUMMARY, "delivered": 1}
    )
    shipment_rows: list[FixtureShipment] | None = None
    role: str = "integration"
    version: str = ""
    delay_seconds: float = 0
    event_read_started: asyncio.Event = field(default_factory=asyncio.Event)

    def selected_shipments(self, query: dict[str, str]) -> list[FixtureShipment]:
        """Honor the backend's public list filters over independently owned rows."""
        rows = self.shipment_rows or []
        rows = [
            row
            for row in rows
            if not row.archived
            and (not query.get("direction") or row.direction == query["direction"])
            and (not query.get("status") or row.status == query["status"])
        ]
        if "delivered_after" in query:
            after = int(query["delivered_after"])
            before = int(query["delivered_before"])
            rows = [
                row
                for row in rows
                if row.delivered_at is not None
                and after
                <= datetime.fromisoformat(row.delivered_at).timestamp()
                < before
            ]
        if query.get("sort") == "delivered":
            rows.sort(
                key=lambda row: (
                    datetime.fromisoformat(row.delivered_at).timestamp()
                    if row.delivered_at
                    else float("-inf")
                ),
                reverse=True,
            )
        elif query.get("sort") == "expected":
            rows.sort(
                key=lambda row: (
                    row.estimated_delivery is None,
                    row.estimated_delivery or "",
                )
            )
        return rows

    def shipment_page(self, query: dict[str, str]) -> dict[str, object]:
        if self.shipment_rows is not None:
            rows = self.selected_shipments(query)
        elif "delivered_after" in query or query.get("status") == "out_for_delivery":
            # Most adapter tests set only counts. Supply matching real-shaped rows
            # unless a test explicitly owns the list contents or HTTP response.
            today = "delivered_after" in query
            count = (
                self.delivery_summary["delivered"]
                if today
                else self.summaries[query.get("direction", "")]["out_for_delivery"]
            )
            start = (
                datetime.fromtimestamp(int(query["delivered_after"]), UTC)
                if today
                else datetime(2026, 1, 1, tzinfo=UTC)
            )
            rows = [
                FixtureShipment(
                    id=str(UUID(int=UUID(SHIPMENT_ID).int + index)),
                    status="delivered" if today else "out_for_delivery",
                    delivered_at=(start + timedelta(seconds=index)).isoformat()
                    if today
                    else None,
                )
                for index in range(count)
            ]
            if today:
                rows.reverse()
        else:
            rows = []
        limit = int(query.get("limit", "20"))
        return {
            "items": [row.record() for row in rows[:limit]],
            "next_page": "fixture-next-page" if len(rows) > limit else None,
        }

    async def handle(self, request: web.Request) -> web.Response:
        if request.path == "/v1/events":
            self.event_read_started.set()
        self.requests.append(
            ReceivedRequest(
                request.method,
                request.path,
                dict(request.query),
                dict(request.headers),
                await request.read(),
            )
        )
        if self.delay_seconds:
            await asyncio.sleep(self.delay_seconds)
        response = self.responses.get(
            (request.method, request.path_qs),
            self.responses.get((request.method, request.path)),
        )
        if response:
            status, body, headers = response
            return web.Response(status=status, body=body, headers=headers)
        if request.path == "/v1/system":
            return web.json_response(
                {"credential_role": self.role, "version": self.version}
            )
        if request.path == "/v1/summary":
            if (
                "delivered_after" in request.query
                or "delivered_before" in request.query
            ):
                summary = self.delivery_summary.copy()
                if self.shipment_rows is not None:
                    summary["delivered"] = len(
                        self.selected_shipments(dict(request.query))
                    )
                return web.json_response(summary)
            summary = self.summaries[request.query.get("direction", "")].copy()
            if self.shipment_rows is not None:
                summary["out_for_delivery"] = len(
                    self.selected_shipments(
                        {**request.query, "status": "out_for_delivery"}
                    )
                )
            return web.json_response(summary)
        if request.path == "/v1/shipments" and request.method == "GET":
            return web.json_response(self.shipment_page(dict(request.query)))
        if request.path == "/v1/delivery-estimates":
            return web.json_response({"items": [], "next_page": None})
        if request.path == "/v1/events":
            return web.json_response(
                {
                    "items": [],
                    "next_after": int(request.query.get("after", "0")),
                    "has_more": False,
                }
            )
        return web.json_response(
            {"id": SHIPMENT_ID, "revision": 7, "status": "expected"},
            headers={"ETag": '"7"'},
        )


@pytest.fixture(autouse=True)
def custom_integrations(enable_custom_integrations) -> None:
    """Enable loading the actual integration from custom_components."""


@pytest.fixture
async def hass_user(hass: HomeAssistant, local_auth) -> MockUser:
    """An authenticated HA user without administrator privileges."""
    group = await hass.auth.async_get_group(GROUP_ID_USER)
    assert group is not None
    return MockUser(groups=[group]).add_to_hass(hass)


@pytest.fixture
async def hass_user_token(hass: HomeAssistant, hass_user: MockUser) -> str:
    refresh_token = await hass.auth.async_create_refresh_token(
        hass_user, client_id="https://example.invalid"
    )
    return hass.auth.async_create_access_token(refresh_token)


@pytest.fixture
async def backend(hass, aiohttp_server, socket_enabled) -> AsyncIterator[Backend]:
    upstream = Backend()
    upstream.version = str((await async_get_integration(hass, DOMAIN)).version)
    app = web.Application()
    app.router.add_route("*", "/{path:.*}", upstream.handle)
    server = await aiohttp_server(app)
    upstream.url = str(server.make_url("")).rstrip("/")
    yield upstream


@pytest.fixture
async def entry(hass: HomeAssistant, backend: Backend) -> MockConfigEntry:
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={CONF_BACKEND_URL: backend.url, CONF_TOKEN: "test-owner-token"},
    )
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    return entry
