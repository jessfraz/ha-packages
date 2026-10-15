"""Real Home Assistant and HTTP-server fixtures for the thin adapter."""

import asyncio
from collections.abc import AsyncIterator
from dataclasses import dataclass, field

import pytest
from aiohttp import web
from homeassistant.auth.const import GROUP_ID_USER
from homeassistant.core import HomeAssistant
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
    role: str = "integration"
    version: str = ""
    delay_seconds: float = 0
    event_read_started: asyncio.Event = field(default_factory=asyncio.Event)

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
                return web.json_response(self.delivery_summary)
            return web.json_response(self.summaries[request.query.get("direction", "")])
        if request.path == "/v1/shipments" and request.method == "GET":
            return web.json_response({"items": [], "next_page": None})
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
