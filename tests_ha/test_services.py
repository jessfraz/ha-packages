"""Verify actions derive their identity from the authenticated HA context."""

import json

import pytest
from homeassistant.core import Context, HomeAssistant
from homeassistant.exceptions import Unauthorized

from custom_components.shipments.const import DOMAIN

from .conftest import SHIPMENT_ID, Backend


async def test_add_action(
    hass: HomeAssistant, hass_user, entry, backend: Backend
) -> None:
    await hass.services.async_call(
        DOMAIN,
        "add",
        {"payload": {"title": "Office supplies"}, "idempotency_key": "create-one"},
        context=Context(user_id=hass_user.id),
        blocking=True,
    )
    request = next(request for request in backend.requests if request.method == "POST")
    assert request.path == "/v1/shipments"
    assert request.headers["X-Shipments-Actor"] == f"ha:{hass_user.id}"
    assert request.headers["Idempotency-Key"] == "create-one"
    assert json.loads(request.body) == {"title": "Office supplies"}


@pytest.mark.parametrize("with_user", [False, True])
async def test_archive_rejects_missing_or_non_admin_identity(
    hass: HomeAssistant, hass_user, entry, backend: Backend, with_user: bool
) -> None:
    user = hass_user if with_user else None
    count = len(backend.requests)
    with pytest.raises(Unauthorized):
        await hass.services.async_call(
            DOMAIN,
            "archive",
            {"shipment_id": SHIPMENT_ID, "if_match": '"7"'},
            context=Context(user_id=user.id if user else None),
            blocking=True,
        )
    assert len(backend.requests) == count


async def test_admin_override(
    hass: HomeAssistant, hass_admin_user, entry, backend: Backend
) -> None:
    await hass.services.async_call(
        DOMAIN,
        "set_override",
        {
            "shipment_id": SHIPMENT_ID,
            "if_match": '"7"',
            "payload": {"status": "delivered", "reason": "Received in person"},
        },
        context=Context(user_id=hass_admin_user.id),
        blocking=True,
    )
    request = next(request for request in backend.requests if request.method == "PUT")
    assert request.path == f"/v1/shipments/{SHIPMENT_ID}/status-override"
    assert request.headers["If-Match"] == '"7"'
    assert request.headers["X-Shipments-Actor"] == f"ha:{hass_admin_user.id}"
