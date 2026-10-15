"""Exercise native action fields, version checks, responses, and authorization."""

import json
from uuid import UUID

import probatio as vol
import pytest
from homeassistant.core import Context, HomeAssistant
from homeassistant.exceptions import (
    HomeAssistantError,
    ServiceValidationError,
    Unauthorized,
)
from homeassistant.setup import async_setup_component

from custom_components.shipments.const import DOMAIN

from .conftest import SHIPMENT_ID, Backend


@pytest.mark.parametrize("explicit_key", [False, True])
@pytest.mark.parametrize("direction", ["inbound", "outbound"])
async def test_add_action_fields_and_response(
    hass: HomeAssistant,
    hass_user,
    entry,
    backend: Backend,
    explicit_key: bool,
    direction: str,
) -> None:
    fields = {
        "title": "Office supplies",
        "direction": direction,
        "merchant": "Example shop",
        "order_reference": "ORDER-1",
        "tracking_number": "TEST123",
        "carrier": "USPS",
    }
    if explicit_key:
        fields["idempotency_key"] = "create-one"
    backend.responses[("POST", "/v1/shipments")] = (
        201,
        json.dumps(
            {
                "id": SHIPMENT_ID,
                "revision": 7,
                "status": "expected",
                "direction": direction,
            }
        ).encode(),
        {"Content-Type": "application/json"},
    )
    result = await hass.services.async_call(
        DOMAIN,
        "add",
        fields,
        context=Context(user_id=hass_user.id),
        blocking=True,
        return_response=True,
    )
    requests = [request for request in backend.requests if request.method == "POST"]
    assert len(requests) == 1
    request = requests[0]
    assert request.path == "/v1/shipments"
    assert request.headers["X-Shipments-Actor"] == f"ha:{hass_user.id}"
    if explicit_key:
        assert request.headers["Idempotency-Key"] == "create-one"
    else:
        assert UUID(request.headers["Idempotency-Key"]).version == 4
    assert json.loads(request.body) == {
        "title": "Office supplies",
        "direction": direction,
        "merchant": "Example shop",
        "order_reference": "ORDER-1",
        "tracking": {"number": "TEST123", "carrier": "USPS"},
    }
    assert result == {"id": SHIPMENT_ID, "revision": 7, "status": "expected"}


@pytest.mark.parametrize("tracking_number", [None, "", " \t\r\n", "\u00a0\u2003\u202f"])
async def test_add_requires_nonblank_tracking_before_transport(
    hass: HomeAssistant,
    hass_user,
    entry,
    backend: Backend,
    tracking_number: str | None,
) -> None:
    fields = {"title": "Expected parcel", "direction": "inbound"}
    if tracking_number is not None:
        fields["tracking_number"] = tracking_number
    count = len(backend.requests)
    with pytest.raises(vol.Invalid):
        await hass.services.async_call(
            DOMAIN,
            "add",
            fields,
            context=Context(user_id=hass_user.id),
            blocking=True,
        )
    assert len(backend.requests) == count


async def test_add_requires_direction_before_transport(
    hass: HomeAssistant, hass_user, entry, backend: Backend
) -> None:
    count = len(backend.requests)
    with pytest.raises(vol.Invalid, match="Direction is required"):
        await hass.services.async_call(
            DOMAIN,
            "add",
            {"title": "Expected parcel", "tracking_number": "TEST123"},
            context=Context(user_id=hass_user.id),
            blocking=True,
        )
    assert len(backend.requests) == count


@pytest.mark.parametrize("direction", [None, "", "sideways", "INBOUND"])
async def test_add_rejects_unsupported_direction_before_transport(
    hass: HomeAssistant,
    hass_user,
    entry,
    backend: Backend,
    direction: str | None,
) -> None:
    count = len(backend.requests)
    with pytest.raises(vol.Invalid, match="Direction must be inbound or outbound"):
        await hass.services.async_call(
            DOMAIN,
            "add",
            {
                "title": "Expected parcel",
                "tracking_number": "TEST123",
                "direction": direction,
            },
            context=Context(user_id=hass_user.id),
            blocking=True,
        )
    assert len(backend.requests) == count


async def test_add_tracked_shipment_without_requesting_response(
    hass: HomeAssistant, hass_user, entry, backend: Backend
) -> None:
    result = await hass.services.async_call(
        DOMAIN,
        "add",
        {
            "title": "Expected parcel",
            "direction": "inbound",
            "tracking_number": " \u2003TEST123\u00a0 ",
        },
        context=Context(user_id=hass_user.id),
        blocking=True,
    )
    request = next(request for request in backend.requests if request.method == "POST")
    assert json.loads(request.body)["tracking"] == {
        "number": "TEST123",
        "carrier": None,
    }
    assert result is None


async def test_action_reports_public_backend_error(
    hass: HomeAssistant, hass_user, entry, backend: Backend
) -> None:
    backend.responses[("POST", "/v1/shipments")] = (
        422,
        json.dumps(
            {
                "message": (
                    "EasyPost does not support this carrier. "
                    "Check the tracking number and carrier."
                ),
                "error_code": "tracking_unsupported",
                "private_details": "private-provider-credential",
            }
        ).encode(),
        {"Content-Type": "application/json"},
    )
    with pytest.raises(HomeAssistantError) as caught:
        await hass.services.async_call(
            DOMAIN,
            "add",
            {
                "title": "Expected parcel",
                "direction": "inbound",
                "tracking_number": "TEST123",
            },
            context=Context(user_id=hass_user.id),
            blocking=True,
        )
    assert "HTTP 422" in str(caught.value)
    assert "tracking_unsupported" in str(caught.value)
    assert "Check the tracking number and carrier." in str(caught.value)
    assert "private-provider-credential" not in str(caught.value)


@pytest.mark.parametrize(
    "body",
    [
        b"not a JSON response",
        b'["not", "an", "error"]',
        b'{"message": {"nested": "private-provider-credential"}}',
        json.dumps({"message": "x" * 501}).encode(),
        json.dumps({"message": "private-provider-credential\nlog line"}).encode(),
        json.dumps(
            {"message": "Bearer test-owner-token", "error_code": "test-owner-token"}
        ).encode(),
    ],
)
async def test_action_error_falls_back_without_reflecting_private_data(
    hass: HomeAssistant, hass_user, entry, backend: Backend, body: bytes
) -> None:
    backend.responses[("POST", "/v1/shipments")] = (503, body, {})
    with pytest.raises(HomeAssistantError) as caught:
        await hass.services.async_call(
            DOMAIN,
            "add",
            {
                "title": "Expected parcel",
                "direction": "inbound",
                "tracking_number": "TEST123",
            },
            context=Context(user_id=hass_user.id),
            blocking=True,
        )
    assert str(caught.value) == "The Shipments backend rejected the action (HTTP 503)"


@pytest.mark.parametrize("with_user", [False, True])
@pytest.mark.parametrize(
    "action", ["archive", "unarchive", "set_override", "clear_override"]
)
async def test_human_decisions_reject_before_any_backend_request(
    hass: HomeAssistant,
    hass_user,
    entry,
    backend: Backend,
    with_user: bool,
    action: str,
) -> None:
    fields = {"shipment_id": SHIPMENT_ID}
    if action == "set_override":
        fields.update(status="delivered", reason="Received in person")
    count = len(backend.requests)
    with pytest.raises(Unauthorized):
        await hass.services.async_call(
            DOMAIN,
            action,
            fields,
            context=Context(user_id=hass_user.id if with_user else None),
            blocking=True,
        )
    assert len(backend.requests) == count


@pytest.mark.parametrize("explicit_version", [False, True])
async def test_admin_override_uses_current_or_explicit_version_once(
    hass: HomeAssistant,
    hass_admin_user,
    entry,
    backend: Backend,
    explicit_version: bool,
) -> None:
    fields = {
        "shipment_id": SHIPMENT_ID,
        "status": "delivered",
        "reason": "Received in person",
    }
    if explicit_version:
        fields["if_match"] = '"4"'
    result = await hass.services.async_call(
        DOMAIN,
        "set_override",
        fields,
        context=Context(user_id=hass_admin_user.id),
        blocking=True,
        return_response=True,
    )
    requests = [request for request in backend.requests if SHIPMENT_ID in request.path]
    assert [request.method for request in requests] == (
        ["PUT"] if explicit_version else ["GET", "PUT"]
    )
    mutation = requests[-1]
    assert mutation.path == f"/v1/shipments/{SHIPMENT_ID}/status-override"
    assert mutation.headers["If-Match"] == ('"4"' if explicit_version else '"7"')
    assert mutation.headers["X-Shipments-Actor"] == f"ha:{hass_admin_user.id}"
    assert json.loads(mutation.body) == {
        "status": "delivered",
        "reason": "Received in person",
    }
    assert result == {"id": SHIPMENT_ID, "revision": 7, "status": "expected"}


@pytest.mark.parametrize("explicit_version", [False, True])
async def test_conflicting_action_is_never_retried(
    hass: HomeAssistant,
    hass_admin_user,
    entry,
    backend: Backend,
    explicit_version: bool,
) -> None:
    backend.responses[("PUT", f"/v1/shipments/{SHIPMENT_ID}/archive")] = (
        412,
        (
            b'{"message":"Review the latest shipment revision.",'
            b'"error_code":"revision_conflict"}'
        ),
        {},
    )
    fields = {"shipment_id": SHIPMENT_ID}
    if explicit_version:
        fields["if_match"] = '"3"'
    with pytest.raises(HomeAssistantError, match="HTTP 412") as caught:
        await hass.services.async_call(
            DOMAIN,
            "archive",
            fields,
            context=Context(user_id=hass_admin_user.id),
            blocking=True,
        )
    assert "revision_conflict" in str(caught.value)
    assert "Review the latest shipment revision." in str(caught.value)
    requests = [request for request in backend.requests if SHIPMENT_ID in request.path]
    assert [request.method for request in requests] == (
        ["PUT"] if explicit_version else ["GET", "PUT"]
    )


@pytest.mark.parametrize("status,headers", [(404, {}), (200, {})])
async def test_failed_version_lookup_never_mutates(
    hass: HomeAssistant, hass_admin_user, entry, backend: Backend, status: int, headers
) -> None:
    backend.responses[("GET", f"/v1/shipments/{SHIPMENT_ID}")] = (
        status,
        b"{}",
        headers,
    )
    with pytest.raises(HomeAssistantError):
        await hass.services.async_call(
            DOMAIN,
            "archive",
            {"shipment_id": SHIPMENT_ID},
            context=Context(user_id=hass_admin_user.id),
            blocking=True,
        )
    requests = [request for request in backend.requests if SHIPMENT_ID in request.path]
    assert [request.method for request in requests] == ["GET"]


async def test_actions_exist_without_a_loaded_entry(
    hass: HomeAssistant, hass_user, backend: Backend
) -> None:
    assert await async_setup_component(hass, DOMAIN, {})
    assert hass.services.has_service(DOMAIN, "add")
    with pytest.raises(ServiceValidationError, match="Shipments is not loaded"):
        await hass.services.async_call(
            DOMAIN,
            "add",
            {
                "title": "Office supplies",
                "direction": "inbound",
                "tracking_number": "TEST123",
            },
            context=Context(user_id=hass_user.id),
            blocking=True,
        )
    assert not backend.requests


async def test_actions_remain_available_after_unload(
    hass: HomeAssistant, hass_user, entry, backend: Backend
) -> None:
    assert await hass.config_entries.async_unload(entry.entry_id)
    count = len(backend.requests)
    assert hass.services.has_service(DOMAIN, "add")
    with pytest.raises(ServiceValidationError, match="Shipments is not loaded"):
        await hass.services.async_call(
            DOMAIN,
            "add",
            {
                "title": "Office supplies",
                "direction": "inbound",
                "tracking_number": "TEST123",
            },
            context=Context(user_id=hass_user.id),
            blocking=True,
        )
    assert len(backend.requests) == count
