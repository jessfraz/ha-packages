"""Exercise native Assist recognition through the loaded integration."""

import json
from dataclasses import dataclass, field
from typing import Any

import pytest
from homeassistant.components import conversation
from homeassistant.components.conversation.trigger import (
    TRIGGER_SCHEMA,
    async_attach_trigger,
)
from homeassistant.components.homeassistant import exposed_entities
from homeassistant.core import Context, HomeAssistant
from homeassistant.helpers import intent
from homeassistant.helpers.script import ScriptRunResult
from homeassistant.setup import async_setup_component
from pytest_homeassistant_custom_component.common import MockConfigEntry, MockUser

from .conftest import Backend, FixtureShipment


@dataclass
class RecordingHandler(intent.IntentHandler):
    """Observe the real native sentence-to-intent boundary."""

    intent_type = "ShipmentsQuery"
    requests: list[intent.Intent] = field(default_factory=list)

    async def async_handle(self, intent_obj: intent.Intent) -> intent.IntentResponse:
        self.requests.append(intent_obj)
        response = intent_obj.create_response()
        response.async_set_speech("A shipment answer")
        return response


@pytest.mark.parametrize(
    ("sentence", "query", "direction", "day", "name"),
    [
        ("What packages are arriving today?", "arriving", "inbound", "today", None),
        (
            "Which shipments are coming tomorrow?",
            "arriving",
            "inbound",
            "tomorrow",
            None,
        ),
        (
            "What outbound deliveries will arrive tomorrow?",
            "arriving",
            "outbound",
            "tomorrow",
            None,
        ),
        (
            "Which inbound and outbound packages are being delivered today?",
            "arriving",
            "all",
            "today",
            None,
        ),
        ("What's out for delivery?", "out_for_delivery", "inbound", "today", None),
        (
            "Which outgoing shipments are out for delivery?",
            "out_for_delivery",
            "outbound",
            "today",
            None,
        ),
        ("What arrived today?", "delivered", "inbound", "today", None),
        ("What was delivered today?", "delivered", "inbound", "today", None),
        (
            "Which outbound shipments were delivered today?",
            "delivered",
            "outbound",
            "today",
            None,
        ),
        ("Are there any tracking problems?", "issues", "inbound", "today", None),
        ("Are any inbound packages delayed?", "issues", "inbound", "today", None),
        (
            "Are there any shipment errors in either direction?",
            "issues",
            "all",
            "today",
            None,
        ),
        (
            "Where is my shipment Office supplies?",
            "status",
            "inbound",
            "today",
            "Office supplies",
        ),
        (
            "When is my package Office supplies arriving?",
            "status",
            "inbound",
            "today",
            "Office supplies",
        ),
        (
            "When is my Office supplies arriving?",
            "status",
            "inbound",
            "today",
            "Office supplies",
        ),
        (
            "When will my Office supplies arrive?",
            "status",
            "inbound",
            "today",
            "Office supplies",
        ),
        (
            "Where is my inbound shipment Office supplies?",
            "status",
            "inbound",
            "today",
            "Office supplies",
        ),
        (
            "When is my inbound package Office supplies arriving?",
            "status",
            "inbound",
            "today",
            "Office supplies",
        ),
        (
            "Where is my outbound shipment Office supplies?",
            "status",
            "outbound",
            "today",
            "Office supplies",
        ),
        (
            "When is my outbound package Office supplies arriving?",
            "status",
            "outbound",
            "today",
            "Office supplies",
        ),
        (
            "When will my outgoing delivery Office supplies be delivered?",
            "status",
            "outbound",
            "today",
            "Office supplies",
        ),
        (
            "Where is my package Office supplies in either direction?",
            "status",
            "all",
            "today",
            "Office supplies",
        ),
        (
            "When is my package Office supplies arriving in either direction?",
            "status",
            "all",
            "today",
            "Office supplies",
        ),
        (
            "When will my package Office supplies arrive in either direction?",
            "status",
            "all",
            "today",
            "Office supplies",
        ),
    ],
)
async def test_native_assist_dispatches_typed_query_slots(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    hass_user: MockUser,
    sentence: str,
    query: str,
    direction: str,
    day: str,
    name: str | None,
) -> None:
    """Normal local conversation reaches our handler with the original caller."""
    handler = RecordingHandler()
    intent.async_register(hass, handler)
    context = Context(user_id=hass_user.id)
    result = await conversation.async_converse(
        hass,
        sentence,
        None,
        context,
        language="en",
        device_id="voice-device",
        satellite_id="assist_satellite.test",
    )
    assert result.response.speech["plain"]["speech"] == "A shipment answer"
    assert len(handler.requests) == 1
    request = handler.requests[0]
    expected_slots = {
        "query": {"value": query},
        "direction": {"value": direction},
        "day": {"value": day},
    }
    if name is not None:
        expected_slots["name"] = {"value": name}
    assert request.slots == expected_slots
    assert request.context == context
    assert request.context.user_id == hass_user.id
    assert request.device_id == "voice-device"
    assert request.satellite_id == "assist_satellite.test"
    assert request.language == "en"
    assert request.assistant == "conversation"
    assert request.conversation_agent_id == conversation.HOME_ASSISTANT_AGENT


async def test_unload_and_reload_keep_one_native_registration(
    hass: HomeAssistant, entry: MockConfigEntry
) -> None:
    """Entry unload removes its sentences; reload does not duplicate callbacks."""
    handler = RecordingHandler()
    intent.async_register(hass, handler)
    await conversation.async_converse(
        hass, "what arrived today", None, Context(), language="en"
    )
    assert len(handler.requests) == 1

    assert await hass.config_entries.async_unload(entry.entry_id)
    result = await conversation.async_converse(
        hass, "what arrived today", None, Context(), language="en"
    )
    assert result.response.response_type is intent.IntentResponseType.ERROR
    assert len(handler.requests) == 1

    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    intent.async_register(hass, handler)
    result = await conversation.async_converse(
        hass, "what arrived today", None, Context(), language="en"
    )
    assert result.response.speech["plain"]["speech"] == "A shipment answer"
    assert len(handler.requests) == 2


async def test_native_question_speaks_the_real_shipment_answer(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    hass_user: MockUser,
    backend: Backend,
) -> None:
    """A spoken question uses the real handler and HTTP-backed shipment query."""
    assert await async_setup_component(hass, "conversation", {})
    exposed_entities.async_expose_entity(
        hass, "conversation", "sensor.shipments_inbound_active", True
    )
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
    result = await conversation.async_converse(
        hass,
        "what packages are out for delivery",
        None,
        Context(user_id=hass_user.id),
        language="en",
    )
    speech = result.response.speech["plain"]["speech"]
    assert "Coffee beans" in speech
    assert "1 inbound shipment" in speech
    assert "PRIVATE" not in speech
    assert backend.requests
    assert all(
        request.method == "GET" and request.path == "/v1/shipments"
        for request in backend.requests
    )


async def test_unrelated_sentences_survive_and_names_are_scoped(
    hass: HomeAssistant, entry: MockConfigEntry, backend: Backend
) -> None:
    """Shipment wildcards do not consume general location questions."""

    async def answer_other(
        run_variables: dict[str, Any], context: Context | None = None
    ) -> ScriptRunResult:
        return ScriptRunResult("An unrelated answer", None, {})

    remove = await async_attach_trigger(
        hass,
        TRIGGER_SCHEMA({"platform": "conversation", "command": ["where is my phone"]}),
        answer_other,
        {
            "domain": "test",
            "name": "Unrelated question",
            "variables": {},
            "trigger_data": {"id": "other", "idx": "0", "alias": None},
        },
    )
    try:
        handler = RecordingHandler()
        intent.async_register(hass, handler)
        for unloaded in (False, True):
            if unloaded:
                assert await hass.config_entries.async_unload(entry.entry_id)
            requests_before = len(backend.requests)
            result = await conversation.async_converse(
                hass, "where is my phone", None, Context(), language="en"
            )
            assert result.response.speech["plain"]["speech"] == "An unrelated answer"
            assert handler.requests == []
            assert len(backend.requests) == requests_before
    finally:
        remove()
