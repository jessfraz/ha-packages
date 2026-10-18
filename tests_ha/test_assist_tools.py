"""Real intent handlers and contributed Assist tools share safe shipment answers."""

import json

import pytest
from homeassistant.components.homeassistant import exposed_entities
from homeassistant.core import Context, HomeAssistant
from homeassistant.helpers import intent, llm
from homeassistant.setup import async_setup_component

from .conftest import Backend, FixtureShipment

GATEWAY = "sensor.shipments_inbound_active"
TOOL = "shipments__query"


async def assist_api(hass: HomeAssistant, user_id: str) -> llm.APIInstance:
    assert await async_setup_component(hass, "llm", {})
    return await llm.async_get_api(
        hass,
        "assist",
        llm.LLMContext(
            platform="test_conversation",
            context=Context(user_id=user_id),
            language="en",
            assistant="conversation",
            device_id=None,
        ),
    )


async def test_real_assist_api_discovers_tool_only_when_exposed(
    hass: HomeAssistant, entry, hass_user
) -> None:
    exposed_entities.async_expose_entity(hass, "conversation", GATEWAY, False)
    assert TOOL not in {
        tool.name for tool in (await assist_api(hass, hass_user.id)).tools
    }
    exposed_entities.async_expose_entity(hass, "conversation", GATEWAY, True)
    api = await assist_api(hass, hass_user.id)
    tool = next(tool for tool in api.tools if tool.name == TOOL)
    assert tool.integration == "shipments"
    assert tool.annotations.read_only
    assert tool.annotations.idempotent
    assert not tool.annotations.destructive
    assert not tool.annotations.open_world
    assert await hass.config_entries.async_unload(entry.entry_id)
    assert TOOL not in {
        tool.name for tool in (await assist_api(hass, hass_user.id)).tools
    }


async def test_native_intent_and_actual_tool_return_the_same_safe_answer(
    hass: HomeAssistant, entry, hass_user, backend: Backend
) -> None:
    exposed_entities.async_expose_entity(hass, "conversation", GATEWAY, True)
    backend.responses[("GET", "/v1/shipments")] = (
        200,
        json.dumps(
            {
                "items": [FixtureShipment(title="Coffee beans").record()],
                "next_page": None,
            }
        ).encode(),
        {},
    )
    backend.requests.clear()
    native = await intent.async_handle(
        hass,
        "conversation",
        "ShipmentsQuery",
        {"query": {"value": "out_for_delivery"}},
        context=Context(user_id=hass_user.id),
        language="en",
        assistant="conversation",
    )
    api = await assist_api(hass, hass_user.id)
    result = await api.async_call_tool(
        llm.ToolInput(TOOL, {"query": "out_for_delivery"})
    )
    native_data = native.as_dict()
    assert native_data["response_type"] == "query_answer"
    assert result.data["speech"] == native_data["speech"]
    assert "Coffee beans" in native_data["speech"]["plain"]["speech"]
    payload = json.dumps(result.data)
    for private in (
        "PRIVATE-TRACKING",
        "PRIVATE-ORDER",
        "PRIVATE-SOURCE",
        "Private recipient",
        "test-owner-token",
    ):
        assert private not in payload
    assert all(
        request.method == "GET" and request.path == "/v1/shipments"
        for request in backend.requests
    )


async def test_tool_revalidates_arguments_through_the_native_handler(
    hass: HomeAssistant, entry, hass_user, backend: Backend
) -> None:
    exposed_entities.async_expose_entity(hass, "conversation", GATEWAY, True)
    api = await assist_api(hass, hass_user.id)
    backend.requests.clear()
    for args in (
        {"query": "delete"},
        {"query": "status", "direction": "sideways"},
        {"query": "arriving", "day": "yesterday"},
    ):
        with pytest.raises(intent.InvalidSlotInfo):
            await api.async_call_tool(llm.ToolInput(TOOL, args))
    assert backend.requests == []


async def test_real_tool_reports_backend_failure_without_exposing_response(
    hass: HomeAssistant, entry, hass_user, backend: Backend
) -> None:
    exposed_entities.async_expose_entity(hass, "conversation", GATEWAY, True)
    backend.responses[("GET", "/v1/shipments")] = (503, b"SECRET provider details", {})
    api = await assist_api(hass, hass_user.id)
    result = await api.async_call_tool(
        llm.ToolInput(TOOL, {"query": "out_for_delivery"})
    )
    assert result.data["response_type"] == "error"
    payload = json.dumps(result.data)
    assert "SECRET" not in payload
    assert "No inbound" not in payload
    assert "unavailable" in payload.lower() or "could not" in payload.lower()
