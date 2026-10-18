"""Entry-owned native Assist sentences for read-only shipment questions."""

from dataclasses import dataclass
from typing import Any, Literal

from homeassistant.config_entries import ConfigEntry
from homeassistant.core import Context, HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import intent
from homeassistant.helpers.script import ScriptRunResult
from homeassistant.helpers.trigger import TriggerActionType


@dataclass(frozen=True)
class SentenceGroup:
    """A fixed query with only the shipment name captured from speech."""

    sentences: tuple[str, ...]
    query: Literal["arriving", "out_for_delivery", "delivered", "status", "issues"]
    direction: Literal["inbound", "outbound", "all"] = "inbound"
    day: Literal["today", "tomorrow"] = "today"


def _sentence_groups() -> tuple[SentenceGroup, ...]:
    groups: list[SentenceGroup] = []
    named_sentences = [
        "where is [my] (package|shipment|delivery) {all_name} in either direction",
        "when is [my] (package|shipment|delivery) {all_name} arriving "
        "in either direction",
        "when will [my] (package|shipment|delivery) {all_name} "
        "(arrive|be delivered) in either direction",
    ]
    for direction, qualifier in (
        ("inbound", "[inbound]"),
        ("outbound", "(outbound|outgoing)"),
        ("all", "(all|inbound and outbound)"),
    ):
        noun = f"{qualifier} (packages|shipments|deliveries)"
        for day in ("today", "tomorrow"):
            groups.append(
                SentenceGroup(
                    (
                        f"(what|which) {noun} [are] "
                        f"(arriving|coming|being delivered) {day}",
                        f"(what|which) {noun} (will arrive|will be delivered) {day}",
                    ),
                    "arriving",
                    direction,
                    day,
                )
            )
        groups.extend(
            (
                SentenceGroup(
                    (f"(what|which) {noun} [are] out for delivery",),
                    "out_for_delivery",
                    direction,
                ),
                SentenceGroup(
                    (
                        f"(what|which) {noun} (arrived|were delivered) today",
                        f"(what|which) {noun} (have|had) been delivered today",
                    ),
                    "delivered",
                    direction,
                ),
                SentenceGroup(
                    (
                        f"are [there] any {qualifier} (delivery|tracking|shipment) "
                        "(problems|issues|errors|delays)",
                        f"(do|are) any {noun} (have|having) "
                        "(problems|issues|errors|delays)",
                        f"are any {noun} delayed",
                    ),
                    "issues",
                    direction,
                ),
            )
        )
        name = "{" + direction + "_name}"
        singular = f"{qualifier} (package|shipment|delivery)"
        named_sentences.extend(
            (
                f"where is [my] {singular} {name}",
                f"when is [my] {singular} {name} arriving",
                f"when will [my] {singular} {name} (arrive|be delivered)",
            )
        )
    # Keep named forms in one trigger, with explicit scope before free titles.
    # A wildcard must not swallow "package" or "in either direction", and one
    # question must dispatch only one query action.
    named_sentences.extend(
        (
            "where is [my] {inbound_name} (package|shipment|delivery)",
            "when is my {name} arriving",
            "when will my {name} (arrive|be delivered)",
        )
    )
    groups.extend(
        (
            SentenceGroup(("(what is|what's) out for delivery",), "out_for_delivery"),
            SentenceGroup(("what (arrived|was delivered) today",), "delivered"),
            SentenceGroup(
                (
                    "are [there] any (delivery|tracking|shipment) "
                    "(problems|issues|errors|delays) in either direction",
                ),
                "issues",
                "all",
            ),
            SentenceGroup(
                tuple(named_sentences),
                "status",
            ),
        )
    )
    return tuple(groups)


def _query_action(hass: HomeAssistant, group: SentenceGroup) -> TriggerActionType:
    """Adapt HA's native trigger envelope to its normal intent dispatcher."""

    async def handle(
        run_variables: dict[str, Any], context: Context | None = None
    ) -> ScriptRunResult:
        trigger = run_variables["trigger"]
        user_input = trigger["user_input"]
        # Sentence triggers serialize the original context. Retain its user and
        # identity rather than dispatching an unowned, privileged request.
        request_context = Context(**user_input["context"])
        slots = {
            "query": {"value": group.query},
            "direction": {"value": group.direction},
            "day": {"value": group.day},
        }
        if name := trigger["slots"].get("name"):
            slots["name"] = {"value": name}
        for direction in ("inbound", "outbound", "all"):
            if name := trigger["slots"].get(f"{direction}_name"):
                slots["name"] = {"value": name}
                slots["direction"] = {"value": direction}
                break
        response = await intent.async_handle(
            hass,
            "conversation",
            "ShipmentsQuery",
            slots=slots,
            text_input=user_input["text"],
            context=request_context,
            language=user_input["language"],
            assistant="conversation",
            device_id=trigger["device_id"],
            satellite_id=trigger["satellite_id"],
            conversation_agent_id=user_input["agent_id"],
        )
        speech = response.speech.get("plain", {}).get("speech")
        if not isinstance(speech, str):
            raise HomeAssistantError("Shipments did not return a spoken response")
        return ScriptRunResult(
            conversation_response=speech, service_response=None, variables={}
        )

    return handle


async def async_register_sentences(hass: HomeAssistant, entry: ConfigEntry) -> None:
    """Register native sentence triggers without changing the selected agent."""
    # HA installs the conversation dependency before entry setup, not when our
    # config flow is first imported.
    from homeassistant.components.conversation.trigger import (
        TRIGGER_SCHEMA,
        async_attach_trigger,
    )

    for index, group in enumerate(_sentence_groups()):
        config = TRIGGER_SCHEMA(
            {"platform": "conversation", "command": list(group.sentences)}
        )
        remove = await async_attach_trigger(
            hass,
            config,
            _query_action(hass, group),
            {
                "domain": "shipments",
                "name": "Shipment questions",
                "variables": {},
                "trigger_data": {
                    "id": f"{entry.entry_id}_{index}",
                    "idx": str(index),
                    "alias": None,
                },
            },
        )
        entry.async_on_unload(remove)
