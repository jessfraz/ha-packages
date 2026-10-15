"""Real Home Assistant and HTTP-server fixtures for the thin adapter."""

import asyncio
from collections.abc import AsyncIterator
from dataclasses import dataclass, field

import pytest
from aiohttp import web
from homeassistant.auth.const import GROUP_ID_USER
from homeassistant.core import HomeAssistant
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
    role: str = "owner"
    delay_seconds: float = 0

    async def handle(self, request: web.Request) -> web.Response:
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
        if response := self.responses.get((request.method, request.path)):
            status, body, headers = response
            return web.Response(status=status, body=body, headers=headers)
        if request.path == "/v1/system":
            return web.json_response({"credential_role": self.role})
        if request.path == "/v1/summary":
            return web.json_response(SUMMARY)
        return web.json_response({"id": SHIPMENT_ID}, headers={"ETag": '"7"'})


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
async def backend(aiohttp_server, socket_enabled) -> AsyncIterator[Backend]:
    upstream = Backend()
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
    await hass.async_block_till_done()
    return entry
