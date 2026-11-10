"""Authenticated, fixed-destination REST bridge for Home Assistant clients."""

import re
from urllib.parse import urlencode

from aiohttp import ClientError, web
from homeassistant.config_entries import ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.helpers.http import HomeAssistantView

from .client import ResponseTooLarge
from .const import DOMAIN, MAX_REQUEST_BYTES
from .coordinator import PackagesCoordinator

_UUID = r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
_TRACKING = r"[A-Za-z0-9-]{6,100}"
_ROUTES = (
    (re.compile(r"packages"), frozenset({"GET", "POST"}), False),
    (
        re.compile(r"summary|system|carriers|delivery-estimates"),
        frozenset({"GET"}),
        False,
    ),
    (re.compile(rf"packages/{_UUID}"), frozenset({"GET", "PATCH"}), False),
    (re.compile(rf"packages/{_UUID}/events"), frozenset({"GET"}), False),
    (re.compile(r"package-reference"), frozenset({"GET"}), False),
    (re.compile(rf"packages/{_UUID}/(?:sources|refresh)"), frozenset({"POST"}), False),
    (
        re.compile(rf"packages/{_UUID}/(?:status-override|archive)"),
        frozenset({"PUT", "DELETE"}),
        True,
    ),
    (re.compile(r"pickups"), frozenset({"GET", "POST"}), True),
    (re.compile(r"pickups/carriers|pickups/resolve"), frozenset({"GET"}), True),
    (re.compile(r"pickups/locations"), frozenset({"GET", "POST"}), True),
    (re.compile(rf"pickups/locations/{_UUID}"), frozenset({"PUT"}), True),
    (
        re.compile(rf"pickups/parcels/{_TRACKING}"),
        frozenset({"GET", "PUT"}),
        True,
    ),
    (
        re.compile(r"pickups/availability|pickups/preview"),
        frozenset({"POST"}),
        True,
    ),
    (
        re.compile(rf"pickups/batches/{_UUID}|pickups/records/{_UUID}"),
        frozenset({"GET"}),
        True,
    ),
    (
        re.compile(rf"pickups/records/{_UUID}/(?:refresh|cancel|reconcile)"),
        frozenset({"POST"}),
        True,
    ),
)
_LIST_QUERY = {
    "tracking_number",
    "carrier",
    "direction",
    "sort",
    "exceptions",
    "order_reference",
    "source_namespace",
    "source_key",
    "search",
    "archived",
    "active",
    "status",
    "tracking_issues",
    "delivered_after",
    "delivered_before",
    "delivered_on",
    "limit",
    "page_token",
}
_EVENT_QUERY = {"limit", "page_token"}


def loaded_coordinator(hass: HomeAssistant) -> PackagesCoordinator | None:
    """Return the sole loaded backend without retaining an unloaded entry."""
    for entry in hass.config_entries.async_entries(DOMAIN):
        if entry.state is ConfigEntryState.LOADED:
            return entry.runtime_data
    return None


class PackagesView(HomeAssistantView):
    """Expose only the service's documented routes through HA authentication."""

    url = "/api/packages/v1/{path:.*}"
    name = "api:packages:v1"
    requires_auth = True

    def __init__(self, hass: HomeAssistant) -> None:
        self._hass = hass

    async def _proxy(self, request: web.Request, path: str) -> web.Response:
        route = next((route for route in _ROUTES if route[0].fullmatch(path)), None)
        if route is None:
            raise web.HTTPNotFound
        if request.method not in route[1]:
            raise web.HTTPMethodNotAllowed(request.method, route[1])
        allowed_query: set[str] = set()
        if request.method == "GET":
            if path == "packages":
                allowed_query = _LIST_QUERY
            elif path in {"package-reference", "pickups/resolve"}:
                allowed_query = {"reference", "carrier"}
            elif path == "pickups":
                allowed_query = _EVENT_QUERY
            elif path.startswith("pickups/parcels/"):
                allowed_query = {"carrier"}
            elif path == "summary":
                allowed_query = {
                    "direction",
                    "delivered_after",
                    "delivered_before",
                    "delivered_on",
                }
            elif path == "delivery-estimates":
                allowed_query = _EVENT_QUERY
            elif path.endswith("/events"):
                allowed_query = _EVENT_QUERY
        if len(request.query_string) > 4096 or request.query.keys() - allowed_query:
            raise web.HTTPBadRequest(reason="Unsupported query parameters")
        user = request["hass_user"]
        if route[2] and not user.is_admin:
            raise web.HTTPForbidden
        coordinator = loaded_coordinator(self._hass)
        if coordinator is None:
            raise web.HTTPServiceUnavailable
        content_length = request.content_length
        if content_length is not None and content_length > MAX_REQUEST_BYTES:
            raise web.HTTPRequestEntityTooLarge(
                max_size=MAX_REQUEST_BYTES, actual_size=content_length
            )
        payload = bytearray()
        async for chunk in request.content.iter_chunked(16 * 1024):
            payload.extend(chunk)
            if len(payload) > MAX_REQUEST_BYTES:
                raise web.HTTPRequestEntityTooLarge(
                    max_size=MAX_REQUEST_BYTES, actual_size=len(payload)
                )
        # A streamed body can outlive a role change or integration unload.
        # Recheck the actual user and the owning entry before forwarding it.
        current_user = await self._hass.auth.async_get_user(user.id)
        if current_user is None or not current_user.is_active:
            raise web.HTTPForbidden
        if route[2] and not current_user.is_admin:
            raise web.HTTPForbidden
        if loaded_coordinator(self._hass) is not coordinator:
            raise web.HTTPServiceUnavailable
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
        if route[2]:
            current_user = await self._hass.auth.async_get_user(user.id)
            if (
                current_user is None
                or not current_user.is_active
                or not current_user.is_admin
            ):
                raise web.HTTPForbidden
        if loaded_coordinator(self._hass) is not coordinator:
            raise web.HTTPServiceUnavailable
        if request.method != "GET" and response.status < 300:
            # Let polling reconcile native counts without delaying the REST response.
            entry = coordinator.config_entry
            if entry is not None and entry.state is ConfigEntryState.LOADED:
                entry.async_create_background_task(
                    self._hass,
                    coordinator.async_request_refresh(),
                    "Packages refresh after action",
                )
        return web.Response(
            status=response.status, body=response.body, headers=response.headers
        )

    get = _proxy
    post = _proxy
    patch = _proxy
    put = _proxy
    delete = _proxy
