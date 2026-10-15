"""Exercise native action fields, version checks, responses, and authorization."""

import json
from uuid import UUID

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
async def test_add_action_fields_and_response(
    hass: HomeAssistant, hass_user, entry, backend: Backend, explicit_key: bool
) -> None:
    fields = {
        "title": "Office supplies",
        "merchant": "Example shop",
        "order_reference": "ORDER-1",
        "tracking_number": "TEST123",
        "carrier": "USPS",
    }
    if explicit_key:
        fields["idempotency_key"] = "create-one"
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
        "merchant": "Example shop",
        "order_reference": "ORDER-1",
        "tracking": {"number": "TEST123", "carrier": "USPS"},
    }
    assert result == {"id": SHIPMENT_ID, "revision": 7, "status": "expected"}


async def test_add_manual_shipment_without_requesting_response(
    hass: HomeAssistant, hass_user, entry, backend: Backend
) -> None:
    result = await hass.services.async_call(
        DOMAIN,
        "add",
        {"title": "Expected parcel"},
        context=Context(user_id=hass_user.id),
        blocking=True,
    )
    request = next(request for request in backend.requests if request.method == "POST")
    assert json.loads(request.body)["tracking"] is None
    assert result is None


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
        b"{}",
        {},
    )
    fields = {"shipment_id": SHIPMENT_ID}
    if explicit_version:
        fields["if_match"] = '"3"'
    with pytest.raises(HomeAssistantError, match="HTTP 412"):
        await hass.services.async_call(
            DOMAIN,
            "archive",
            fields,
            context=Context(user_id=hass_admin_user.id),
            blocking=True,
        )
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
            {"title": "Office supplies"},
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
            {"title": "Office supplies"},
            context=Context(user_id=hass_user.id),
            blocking=True,
        )
    assert len(backend.requests) == count
