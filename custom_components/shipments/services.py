"""Native Home Assistant actions backed by the same Rust API."""

import json
from uuid import UUID

import probatio as vol
from aiohttp import ClientError
from homeassistant.core import HomeAssistant, ServiceCall
from homeassistant.exceptions import (
    HomeAssistantError,
    ServiceValidationError,
    Unauthorized,
)

from .client import ResponseTooLarge
from .const import DOMAIN, MAX_REQUEST_BYTES
from .http import loaded_coordinator

SERVICE_ACTIONS = {
    "add": ("POST", "/v1/shipments", False, True),
    "refresh": ("POST", "/v1/shipments/{shipment_id}/refresh", False, False),
    "set_override": ("PUT", "/v1/shipments/{shipment_id}/status-override", True, True),
    "clear_override": (
        "DELETE",
        "/v1/shipments/{shipment_id}/status-override",
        True,
        False,
    ),
    "archive": ("PUT", "/v1/shipments/{shipment_id}/archive", True, False),
    "unarchive": ("DELETE", "/v1/shipments/{shipment_id}/archive", True, False),
}


def async_register_services(hass: HomeAssistant) -> None:
    """Register actions once for the loaded backend."""

    async def handle(call: ServiceCall) -> None:
        method, path, admin_only, _ = SERVICE_ACTIONS[call.service]
        user_id = call.context.user_id
        user = await hass.auth.async_get_user(user_id) if user_id else None
        if user is None or not user.is_active or (admin_only and not user.is_admin):
            raise Unauthorized(context=call.context)
        coordinator = loaded_coordinator(hass)
        if coordinator is None:
            raise ServiceValidationError("Shipments is not loaded")
        if "shipment_id" in call.data:
            try:
                shipment_id = str(UUID(call.data["shipment_id"]))
            except ValueError as err:
                raise ServiceValidationError("Invalid shipment ID") from err
            path = path.format(shipment_id=shipment_id)
        body = None
        if "payload" in call.data:
            body = json.dumps(call.data["payload"]).encode()
            if len(body) > MAX_REQUEST_BYTES:
                raise ServiceValidationError("Shipment payload is too large")
        headers = {
            header: call.data[field]
            for field, header in (
                ("if_match", "If-Match"),
                ("idempotency_key", "Idempotency-Key"),
            )
            if field in call.data
        }
        try:
            response = await coordinator.client.request(
                method, path, actor=f"ha:{user.id}", body=body, headers=headers
            )
        except ClientError, TimeoutError, ResponseTooLarge:
            raise HomeAssistantError("Cannot contact the Shipments backend") from None
        if response.status >= 300:
            raise HomeAssistantError(
                f"The Shipments backend rejected the action (HTTP {response.status})"
            )
        await coordinator.async_request_refresh()

    for name, (_, _, _, needs_payload) in SERVICE_ACTIONS.items():
        fields = {
            vol.Required("idempotency_key")
            if name == "add"
            else vol.Optional("idempotency_key"): str,
        }
        if name != "add":
            fields[vol.Required("shipment_id")] = str
            fields[vol.Required("if_match")] = str
        if needs_payload:
            fields[vol.Required("payload")] = dict
        hass.services.async_register(DOMAIN, name, handle, schema=vol.Schema(fields))


def async_remove_services(hass: HomeAssistant) -> None:
    """Remove actions when the backend unloads."""
    for name in SERVICE_ACTIONS:
        hass.services.async_remove(DOMAIN, name)
