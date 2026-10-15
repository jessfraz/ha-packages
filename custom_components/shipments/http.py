"""Authenticated, fixed-destination REST bridge for Home Assistant clients."""

import re
from urllib.parse import urlencode

from aiohttp import ClientError, web
from homeassistant.config_entries import ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.helpers.http import HomeAssistantView

from .client import ResponseTooLarge
from .const import DOMAIN, MAX_REQUEST_BYTES
from .coordinator import ShipmentsCoordinator

_UUID = r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
_ROUTES = (
    (re.compile(r"shipments"), frozenset({"GET", "POST"}), False),
    (re.compile(r"summary|system"), frozenset({"GET"}), False),
    (re.compile(rf"shipments/{_UUID}"), frozenset({"GET", "PATCH"}), False),
    (re.compile(rf"shipments/{_UUID}/events"), frozenset({"GET"}), False),
    (re.compile(rf"shipments/{_UUID}/(?:sources|refresh)"), frozenset({"POST"}), False),
    (
        re.compile(rf"shipments/{_UUID}/(?:status-override|archive)"),
        frozenset({"PUT", "DELETE"}),
        True,
    ),
)
_LIST_QUERY = {
    "tracking_number",
    "carrier",
    "order_reference",
    "source_namespace",
    "source_key",
    "search",
    "archived",
    "status",
    "limit",
    "page_token",
}
_EVENT_QUERY = {"limit", "page_token"}


def loaded_coordinator(hass: HomeAssistant) -> ShipmentsCoordinator | None:
    """Return the sole loaded backend without retaining an unloaded entry."""
    for entry in hass.config_entries.async_entries(DOMAIN):
        if entry.state is ConfigEntryState.LOADED:
            return entry.runtime_data
    return None


class ShipmentsView(HomeAssistantView):
    """Expose only the service's documented routes through HA authentication."""

    url = "/api/shipments/v1/{path:.*}"
    name = "api:shipments:v1"
    requires_auth = True

    def __init__(self, hass: HomeAssistant) -> None:
        self._hass = hass

    async def _proxy(self, request: web.Request, path: str) -> web.Response:
        route = next((route for route in _ROUTES if route[0].fullmatch(path)), None)
        if route is None:
            raise web.HTTPNotFound
        if request.method not in route[1]:
            raise web.HTTPMethodNotAllowed(request.method, route[1])
        allowed_query = (
            (
                _LIST_QUERY
                if path == "shipments"
                else _EVENT_QUERY
                if path.endswith("/events")
                else set()
            )
            if request.method == "GET"
            else set()
        )
        if len(request.query_string) > 4096 or request.query.keys() - allowed_query:
            raise web.HTTPBadRequest(reason="Unsupported query parameters")
        user = request["hass_user"]
        if route[2] and not user.is_admin:
            raise web.HTTPForbidden
        coordinator = loaded_coordinator(self._hass)
        if coordinator is None:
            raise web.HTTPServiceUnavailable
        if (request.content_length or 0) > MAX_REQUEST_BYTES:
            raise web.HTTPRequestEntityTooLarge(
                max_size=MAX_REQUEST_BYTES, actual_size=request.content_length
            )
        payload = bytearray()
        async for chunk in request.content.iter_chunked(16 * 1024):
            payload.extend(chunk)
            if len(payload) > MAX_REQUEST_BYTES:
                raise web.HTTPRequestEntityTooLarge(
                    max_size=MAX_REQUEST_BYTES, actual_size=len(payload)
                )
        headers = {
            key: request.headers[key]
            for key in ("If-Match", "If-None-Match", "Idempotency-Key")
            if key in request.headers
        }
        try:
            response = await coordinator.client.request(
                request.method,
                f"/v1/{path}"
                + (
                    f"?{urlencode(list(request.query.items()))}"
                    if request.query
                    else ""
                ),
                body=bytes(payload) or None,
                actor=f"ha:{user.id}",
                headers=headers,
            )
        except TimeoutError:
            raise web.HTTPGatewayTimeout from None
        except ClientError, ResponseTooLarge:
            raise web.HTTPBadGateway from None
        if request.method != "GET" and response.status < 300:
            # Let polling reconcile native counts without delaying the REST response.
            self._hass.async_create_task(coordinator.async_request_refresh())
        return web.Response(
            status=response.status, body=response.body, headers=response.headers
        )

    get = _proxy
    post = _proxy
    patch = _proxy
    put = _proxy
    delete = _proxy
