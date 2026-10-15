"""Native Home Assistant actions backed by the same Rust API."""

import json
from typing import TypedDict
from uuid import UUID, uuid4

import probatio as vol
from aiohttp import ClientError
from homeassistant.core import HomeAssistant, ServiceCall, SupportsResponse
from homeassistant.exceptions import (
    HomeAssistantError,
    ServiceValidationError,
    Unauthorized,
)

from .client import BackendResponse, ResponseTooLarge
from .const import CONF_TOKEN, DOMAIN, MAX_REQUEST_BYTES
from .http import loaded_coordinator

SERVICE_ACTIONS = {
    "add": ("POST", "/v1/shipments", False),
    "refresh": ("POST", "/v1/shipments/{shipment_id}/refresh", False),
    "set_override": ("PUT", "/v1/shipments/{shipment_id}/status-override", True),
    "clear_override": ("DELETE", "/v1/shipments/{shipment_id}/status-override", True),
    "archive": ("PUT", "/v1/shipments/{shipment_id}/archive", True),
    "unarchive": ("DELETE", "/v1/shipments/{shipment_id}/archive", True),
}


class ShipmentActionResponse(TypedDict):
    """Identity and resulting state returned to a calling HA script."""

    id: str
    revision: int
    status: str


def _check_response(response: BackendResponse, *, credential: str) -> None:
    if 200 <= response.status < 300:
        return
    if response.status == 412:
        description = (
            "The shipment changed before this action could be saved (HTTP 412). "
            "Review the current shipment before trying again."
        )
    else:
        description = (
            f"The Shipments backend rejected the action (HTTP {response.status})"
        )
    try:
        data = response.json()
    except ValueError:
        data = None
    if isinstance(data, dict):
        code = data.get("error_code")
        message = data.get("message")
        if (
            isinstance(code, str)
            and 0 < len(code) <= 100
            and code.isascii()
            and all(character.isalnum() or character in "_.-" for character in code)
            and (not credential or credential not in code)
        ):
            description += f" [{code}]"
        if (
            isinstance(message, str)
            and 0 < len(message.strip()) <= 500
            and message.isprintable()
            and (not credential or credential not in message)
        ):
            description += f": {message.strip()}"
    raise HomeAssistantError(description)


def _response_data(response: BackendResponse) -> ShipmentActionResponse:
    try:
        data = response.json()
    except ValueError:
        raise HomeAssistantError("The backend returned an invalid shipment") from None
    if (
        not isinstance(data, dict)
        or not isinstance(data.get("id"), str)
        or type(data.get("revision")) is not int
        or not isinstance(data.get("status"), str)
    ):
        raise HomeAssistantError("The backend returned an invalid shipment")
    return {"id": data["id"], "revision": data["revision"], "status": data["status"]}


def async_register_services(hass: HomeAssistant) -> None:
    """Keep actions available for editing even when the backend is unloaded."""

    async def handle(call: ServiceCall) -> ShipmentActionResponse | None:
        method, path, admin_only = SERVICE_ACTIONS[call.service]
        user_id = call.context.user_id
        user = await hass.auth.async_get_user(user_id) if user_id else None
        if user is None or not user.is_active or (admin_only and not user.is_admin):
            raise Unauthorized(context=call.context)
        coordinator = loaded_coordinator(hass)
        if coordinator is None:
            raise ServiceValidationError("Shipments is not loaded")
        actor = f"ha:{user.id}"
        credential = (
            coordinator.config_entry.data.get(CONF_TOKEN, "")
            if coordinator.config_entry
            else ""
        )
        shipment_path = None
        if call.service != "add":
            try:
                shipment_id = str(UUID(call.data["shipment_id"]))
            except ValueError as err:
                raise ServiceValidationError("Invalid shipment ID") from err
            shipment_path = f"/v1/shipments/{shipment_id}"
            path = path.format(shipment_id=shipment_id)

        payload: dict[str, object] | None = None
        if call.service == "add":
            recipient_url = call.data.get("recipient_url") or None
            if (
                call.data.get("carrier", "").strip().casefold() == "doordash"
                and not recipient_url
            ):
                raise ServiceValidationError(
                    "DoorDash requires its original recipient tracking link. "
                    "Copy the link from the merchant's shipping email into "
                    "recipient_url."
                )
            payload = {
                "title": call.data["title"],
                "direction": call.data["direction"],
                "merchant": call.data.get("merchant") or None,
                "order_reference": call.data.get("order_reference") or None,
                "tracking": {
                    "number": call.data["tracking_number"],
                    "carrier": call.data.get("carrier") or None,
                    "recipient_url": recipient_url,
                },
            }
        elif call.service == "set_override":
            payload = {"status": call.data["status"], "reason": call.data["reason"]}
        body = json.dumps(payload).encode() if payload is not None else None
        if body and len(body) > MAX_REQUEST_BYTES:
            raise ServiceValidationError("Shipment payload is too large")

        headers: dict[str, str] = {}
        if call.service == "add":
            headers["Idempotency-Key"] = (
                call.data["idempotency_key"]
                if "idempotency_key" in call.data
                else str(uuid4())
            )
        elif "if_match" in call.data:
            headers["If-Match"] = call.data["if_match"]
        try:
            if shipment_path and "If-Match" not in headers:
                current = await coordinator.client.request(
                    "GET", shipment_path, actor=actor
                )
                _check_response(current, credential=credential)
                if not (etag := current.headers.get("ETag")):
                    raise HomeAssistantError("The backend omitted the shipment version")
                headers["If-Match"] = etag
            # A concurrent change must fail. Never fetch and retry a rejected write.
            response = await coordinator.client.request(
                method, path, actor=actor, body=body, headers=headers
            )
        except ClientError, TimeoutError, ResponseTooLarge:
            raise HomeAssistantError("Cannot contact the Shipments backend") from None
        _check_response(response, credential=credential)
        await coordinator.async_request_refresh()
        return _response_data(response) if call.return_response else None

    for name in SERVICE_ACTIONS:
        if name == "add":
            fields = {
                vol.Required("title"): str,
                vol.Required(
                    "direction",
                    msg="Direction is required. Choose inbound or outbound.",
                ): vol.In(
                    ("inbound", "outbound"),
                    msg="Direction must be inbound or outbound.",
                ),
                vol.Optional("merchant"): str,
                vol.Optional("order_reference"): str,
                vol.Required("tracking_number"): vol.All(
                    str, str.strip, vol.Length(min=1)
                ),
                vol.Optional("carrier"): str,
                vol.Optional("recipient_url"): vol.All(str, str.strip),
                vol.Optional("idempotency_key"): str,
            }
        else:
            fields = {
                vol.Required("shipment_id"): str,
                vol.Optional("if_match"): str,
            }
            if name == "set_override":
                fields[vol.Required("status")] = str
                fields[vol.Required("reason")] = str
        hass.services.async_register(
            DOMAIN,
            name,
            handle,
            schema=vol.Schema(fields),
            supports_response=SupportsResponse.OPTIONAL,
        )
