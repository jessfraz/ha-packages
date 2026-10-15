"""Exercise HA authentication and the real outbound HTTP transport."""

import json

import pytest
from homeassistant.core import HomeAssistant

from custom_components.shipments.const import MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES

from .conftest import SHIPMENT_ID, Backend

PREFIX = "/api/shipments/v1"


async def test_proxy_identity_headers_and_error_status(
    hass_client, hass_admin_user, entry, backend: Backend
) -> None:
    path = f"/v1/shipments/{SHIPMENT_ID}"
    backend.responses[("PATCH", path)] = (
        412,
        b'{"error_code":"version_conflict"}',
        {"ETag": '"8"', "Content-Type": "application/json", "Set-Cookie": "ignored=x"},
    )
    client = await hass_client()
    response = await client.patch(
        f"{PREFIX}/shipments/{SHIPMENT_ID}",
        json={"title": "Office supplies"},
        headers={
            "If-Match": '"7"',
            "Idempotency-Key": "retry-123",
            "X-Shipments-Actor": "forged-person",
            "X-Caller-Secret": "caller-value",
        },
    )
    assert response.status == 412
    assert response.headers["ETag"] == '"8"'
    assert "Set-Cookie" not in response.headers
    assert await response.json() == {"error_code": "version_conflict"}
    forwarded = backend.requests[-1]
    assert forwarded.headers["Authorization"] == "Bearer test-owner-token"
    assert forwarded.headers["X-Shipments-Actor"] == f"ha:{hass_admin_user.id}"
    assert forwarded.headers["If-Match"] == '"7"'
    assert forwarded.headers["Idempotency-Key"] == "retry-123"
    assert "X-Caller-Secret" not in forwarded.headers
    assert json.loads(forwarded.body) == {"title": "Office supplies"}


async def test_proxy_requires_ha_authentication(hass_client_no_auth, entry) -> None:
    client = await hass_client_no_auth()
    assert (await client.get(f"{PREFIX}/summary")).status == 401


@pytest.mark.parametrize("operation", ["archive", "status-override"])
@pytest.mark.parametrize("method", ["put", "delete"])
async def test_decisions_require_admin(
    hass_client, hass_user_token, entry, backend: Backend, operation: str, method: str
) -> None:
    client = await hass_client(hass_user_token)
    count = len(backend.requests)
    response = await getattr(client, method)(
        f"{PREFIX}/shipments/{SHIPMENT_ID}/{operation}",
        headers={"If-Match": '"1"'},
    )
    assert response.status == 403
    assert len(backend.requests) == count


async def test_non_admin_can_add_and_read(
    hass_client, hass_user, hass_user_token, entry, backend: Backend
) -> None:
    client = await hass_client(hass_user_token)
    response = await client.post(
        f"{PREFIX}/shipments",
        json={"title": "Office supplies"},
        headers={"Idempotency-Key": "create-123"},
    )
    assert response.status == 200
    forwarded = next(
        request for request in backend.requests if request.method == "POST"
    )
    assert forwarded.headers["X-Shipments-Actor"] == f"ha:{hass_user.id}"
    assert (await client.get(f"{PREFIX}/shipments")).status == 200


@pytest.mark.parametrize(
    "method,path,status",
    [
        ("get", "../../system", 404),
        ("get", "https://elsewhere.invalid", 404),
        ("get", "shipments/not-a-uuid", 404),
        ("post", "system", 405),
        ("delete", "shipments", 405),
        ("get", "summary?url=https://elsewhere.invalid", 400),
        ("get", "shipments?destination=https://elsewhere.invalid", 400),
        ("post", f"shipments/{SHIPMENT_ID}/refresh?limit=1", 400),
    ],
)
async def test_fixed_route_allowlist(
    hass_client, entry, backend: Backend, method: str, path: str, status: int
) -> None:
    client = await hass_client()
    count = len(backend.requests)
    response = await getattr(client, method)(f"{PREFIX}/{path}")
    assert response.status == status
    assert len(backend.requests) == count


async def test_list_and_event_query_forwarding(
    hass_client, entry, backend: Backend
) -> None:
    client = await hass_client()
    response = await client.get(
        f"{PREFIX}/shipments",
        params={"search": "book & pen", "limit": "10", "active": "true"},
    )
    assert response.status == 200
    assert backend.requests[-1].query == {
        "search": "book & pen",
        "limit": "10",
        "active": "true",
    }
    response = await client.get(
        f"{PREFIX}/shipments/{SHIPMENT_ID}/events", params={"page_token": "next+one"}
    )
    assert response.status == 200
    assert backend.requests[-1].query == {"page_token": "next+one"}


async def test_upstream_redirect_is_not_followed(
    hass_client, entry, backend: Backend
) -> None:
    backend.responses[("GET", "/v1/shipments")] = (
        302,
        b"",
        {"Location": f"{backend.url}/credential-sink"},
    )
    client = await hass_client()
    response = await client.get(f"{PREFIX}/shipments", allow_redirects=False)
    assert response.status == 302
    assert "Location" not in response.headers
    assert not any(request.path == "/credential-sink" for request in backend.requests)


async def test_request_limit(hass_client, entry, backend: Backend) -> None:
    client = await hass_client()
    count = len(backend.requests)
    response = await client.post(
        f"{PREFIX}/shipments", data=b"x" * (MAX_REQUEST_BYTES + 1)
    )
    assert response.status == 413
    assert len(backend.requests) == count


async def test_response_limit(hass_client, entry, backend: Backend) -> None:
    backend.responses[("GET", "/v1/shipments")] = (
        200,
        b"x" * (MAX_RESPONSE_BYTES + 1),
        {},
    )
    client = await hass_client()
    assert (await client.get(f"{PREFIX}/shipments")).status == 502


async def test_request_limit_on_chunked_body(
    hass_client, entry, backend: Backend
) -> None:
    async def chunks():
        yield b"x" * MAX_REQUEST_BYTES
        yield b"x"

    client = await hass_client()
    count = len(backend.requests)
    assert (await client.post(f"{PREFIX}/shipments", data=chunks())).status == 413
    assert len(backend.requests) == count


async def test_upstream_timeout(
    hass_client, entry, backend: Backend, monkeypatch
) -> None:
    monkeypatch.setattr(
        "custom_components.shipments.client.REQUEST_TIMEOUT_SECONDS", 0.01
    )
    backend.delay_seconds = 0.03
    client = await hass_client()
    assert (await client.get(f"{PREFIX}/shipments")).status == 504


async def test_proxy_unavailable_after_unload(
    hass: HomeAssistant, hass_client, entry
) -> None:
    client = await hass_client()
    assert await hass.config_entries.async_unload(entry.entry_id)
    assert (await client.get(f"{PREFIX}/summary")).status == 503
