"""Actual HA entities and Assist report discovery failures independently of tracking."""

import json

import pytest
from homeassistant.components.homeassistant import exposed_entities
from homeassistant.core import HomeAssistant
from homeassistant.helpers import llm
from homeassistant.util import dt as dt_util

from .conftest import Backend, get_state
from .test_assist_tools import GATEWAY, assist_api

SENSOR = "sensor.packages_amazon_discovery"
PROBLEM = "binary_sensor.packages_amazon_discovery_problem"


def discovery_report(**changes: object) -> dict[str, object]:
    now = int(dt_util.utcnow().timestamp())
    return {
        "state": "complete",
        "problem": False,
        "checked_at": now,
        "last_attempt_at": now - 100,
        "last_finished_at": now - 30,
        "last_complete_at": now - 30,
        "next_attempt_at": now + 1800,
        "running_until": None,
        "consecutive_failures": 0,
        "last_outcome": "complete",
        "last_error_code": None,
        "has_more": False,
        "deferred_parcels": 0,
        "unexpected_secret": "PRIVATE-ACCOUNT-TOKEN",
        **changes,
    }


def report(backend: Backend, discovery: object) -> None:
    backend.responses[("GET", "/v1/system")] = (
        200,
        json.dumps(
            {"version": backend.version, "amazon_discovery": discovery}
        ).encode(),
        {},
    )


async def test_discovery_entities_show_failure_and_recovery_without_parcel_changes(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    report(backend, discovery_report())
    await entry.runtime_data.async_refresh()
    assert get_state(hass, SENSOR).state == "complete"
    assert get_state(hass, PROBLEM).state == "off"
    report(backend, discovery_report(state="stale", problem=True))
    await entry.runtime_data.async_refresh()
    assert get_state(hass, SENSOR).state == "stale"
    assert get_state(hass, PROBLEM).state == "on"
    assert get_state(hass, "sensor.packages_active").state == "4"
    assert "PRIVATE-" not in json.dumps(dict(get_state(hass, SENSOR).attributes))
    report(
        backend,
        discovery_report(
            state="retrying", last_outcome="failed", consecutive_failures=1
        ),
    )
    await entry.runtime_data.async_refresh()
    assert get_state(hass, SENSOR).state == "retrying"
    assert get_state(hass, PROBLEM).state == "off"


@pytest.mark.parametrize(
    "invalid",
    [
        None,
        {},
        {"state": "complete"},
        discovery_report(checked_at=True),
        discovery_report(checked_at=0),
    ],
)
async def test_missing_invalid_or_old_health_is_unavailable(
    hass: HomeAssistant, entry, backend: Backend, invalid: object
) -> None:
    report(backend, invalid)
    await entry.runtime_data.async_refresh()
    assert get_state(hass, SENSOR).state == "unavailable"
    assert get_state(hass, PROBLEM).state == "unavailable"
    assert get_state(hass, "sensor.packages_active").state == "4"


async def test_health_tool_is_read_only_exposed_and_uses_actual_current_report(
    hass: HomeAssistant, entry, backend: Backend, hass_user
) -> None:
    report(
        backend,
        discovery_report(
            state="partial",
            has_more=True,
            last_outcome="partial",
            last_complete_at=None,
        ),
    )
    await entry.runtime_data.async_refresh()
    exposed_entities.async_expose_entity(hass, "conversation", GATEWAY, False)
    assert "packages__health" not in {
        tool.name for tool in (await assist_api(hass, hass_user.id)).tools
    }
    exposed_entities.async_expose_entity(hass, "conversation", GATEWAY, True)
    api = await assist_api(hass, hass_user.id)
    tool = next(tool for tool in api.tools if tool.name == "packages__health")
    assert tool.annotations.read_only
    assert not tool.annotations.destructive
    backend.requests.clear()
    result = await api.async_call_tool(llm.ToolInput("packages__health", {}))
    payload = json.dumps(result.data)
    assert result.data["response_type"] == "query_answer"
    assert "partial" in payload
    assert "No complete scan has been recorded" in payload
    assert "PRIVATE-" not in payload
    assert backend.requests == []
    exposed_entities.async_expose_entity(hass, "conversation", GATEWAY, False)
    hidden = await api.async_call_tool(llm.ToolInput("packages__health", {}))
    assert hidden.data["response_type"] == "error"
    assert "last_complete_at" not in json.dumps(hidden.data)
