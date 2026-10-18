"""Native calendar identity, app destinations and frontend resource lifecycle."""

import json

import pytest
from homeassistant.components import frontend
from homeassistant.core import HomeAssistant
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.shipments.const import (
    CONF_ADDON_SLUG,
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
)

from .conftest import SHIPMENT_ID, Backend, get_state


@pytest.mark.parametrize("supervised", [False, True])
async def test_calendar_links_use_event_identity_and_loaded_app_destination(
    hass: HomeAssistant, hass_client, backend: Backend, supervised: bool
) -> None:
    backend.responses[("GET", "/v1/delivery-estimates?limit=200")] = (
        200,
        json.dumps(
            {
                "items": [
                    {
                        "id": SHIPMENT_ID,
                        "title": "Office supplies & stationery",
                        "carrier": "USPS",
                        "estimated_delivery_at": "2026-10-20",
                    }
                ],
                "next_page": None,
            }
        ).encode(),
        {},
    )
    data = {CONF_BACKEND_URL: backend.url, CONF_TOKEN: "integration-token"}
    if supervised:
        data[CONF_ADDON_SLUG] = "example_shipments"
    entry = MockConfigEntry(domain=DOMAIN, data=data)
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)

    state = get_state(hass, "calendar.shipments_inbound_deliveries")
    assert state.attributes.get("shipments_calendar") is True
    assert state.attributes["shipments_app_url"] == (
        "/shipments/shipments/" if supervised else f"{backend.url}/"
    )
    modules = hass.data[frontend.DATA_EXTRA_MODULE_URL]
    resources = [url for url in modules.urls if url.startswith("/shipments-static/")]
    assert len(resources) == 1
    resource = resources[0]
    client = await hass_client()
    response = await client.get(resource)
    assert response.status == 200
    assert response.content_type in {"text/javascript", "application/javascript"}
    events_response = await client.get(
        "/api/calendars/calendar.shipments_inbound_deliveries",
        params={"start": "2026-10-20T00:00:00Z", "end": "2026-10-22T00:00:00Z"},
    )
    assert events_response.status == 200
    events = await events_response.json()
    assert len(events) == 1
    assert events[0]["uid"] == SHIPMENT_ID
    assert events[0]["summary"] == "Office supplies & stationery"

    assert await hass.config_entries.async_unload(entry.entry_id)
    assert resource not in modules.urls
    state = hass.states.get("calendar.shipments_inbound_deliveries")
    assert state is None or state.state == "unavailable"
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    assert list(modules.urls).count(resource) == 1
    assert (await client.get(resource)).status == 200
