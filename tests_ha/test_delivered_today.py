"""Today's inbound count uses HA's civil day and backend carrier timestamps."""

import json
from datetime import datetime
from urllib.parse import urlencode
from zoneinfo import ZoneInfo

import pytest
from homeassistant.core import HomeAssistant
from homeassistant.helpers import issue_registry as ir
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.shipments.const import (
    CONF_ADDON_SLUG,
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
)

from .conftest import Backend, get_state


@pytest.mark.parametrize(
    ("zone", "now", "after", "before"),
    [
        (
            "America/Los_Angeles",
            "2026-03-08T09:30:00+00:00",
            "2026-03-08T08:00:00+00:00",
            "2026-03-09T07:00:00+00:00",
        ),
        (
            "America/Los_Angeles",
            "2026-11-01T09:30:00+00:00",
            "2026-11-01T07:00:00+00:00",
            "2026-11-02T08:00:00+00:00",
        ),
        (
            "America/Los_Angeles",
            "2026-10-15T01:00:00+00:00",
            "2026-10-14T07:00:00+00:00",
            "2026-10-15T07:00:00+00:00",
        ),
        (
            "Australia/Lord_Howe",
            "2026-10-04T04:00:00+00:00",
            "2026-10-03T13:30:00+00:00",
            "2026-10-04T13:00:00+00:00",
        ),
    ],
)
async def test_inbound_delivered_today_uses_ha_timezone_and_dst(
    hass: HomeAssistant,
    entry,
    backend: Backend,
    freezer,
    zone: str,
    now: str,
    after: str,
    before: str,
) -> None:
    await hass.config.async_set_time_zone(zone)
    freezer.move_to(datetime.fromisoformat(now))
    # Finish refreshes queued by the timezone change before recording this day.
    await hass.async_block_till_done(wait_background_tasks=True)
    backend.delivery_summary["delivered"] = 7
    backend.summaries["inbound"]["delivered"] = 24
    backend.requests.clear()
    await entry.runtime_data.async_refresh()
    today_state = hass.states.get("sensor.shipments_inbound_delivered_today")
    assert today_state is not None, "Inbound delivered-today sensor is missing"
    assert today_state.state == "7"
    queries = [
        request.query
        for request in backend.requests
        if request.path == "/v1/summary" and "delivered_after" in request.query
    ]
    assert queries
    assert all(
        query
        == {
            "direction": "inbound",
            "delivered_after": str(int(datetime.fromisoformat(after).timestamp())),
            "delivered_before": str(int(datetime.fromisoformat(before).timestamp())),
            "delivered_on": datetime.fromisoformat(now)
            .astimezone(ZoneInfo(zone))
            .date()
            .isoformat(),
        }
        for query in queries
    )


async def test_today_count_rolls_over_at_ha_midnight(
    hass: HomeAssistant, entry, backend: Backend, freezer
) -> None:
    await hass.config.async_set_time_zone("America/Los_Angeles")
    freezer.move_to(datetime.fromisoformat("2026-10-15T06:59:59+00:00"))
    backend.delivery_summary["delivered"] = 3
    await entry.runtime_data.async_refresh()
    assert get_state(hass, "sensor.shipments_inbound_delivered_today").state == "3"
    freezer.move_to(datetime.fromisoformat("2026-10-15T07:00:00+00:00"))
    backend.delivery_summary["delivered"] = 0
    backend.requests.clear()
    await entry.runtime_data.async_refresh()
    assert get_state(hass, "sensor.shipments_inbound_delivered_today").state == "0"
    today = next(
        request for request in backend.requests if "delivered_after" in request.query
    )
    assert today.query["delivered_after"] == "1792047600"
    assert today.query["delivered_before"] == "1792134000"


async def test_failed_today_count_does_not_publish_partial_sensor_snapshot(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    previous = entry.runtime_data.data.copy()
    today = next(
        request for request in backend.requests if "delivered_after" in request.query
    )
    backend.responses[("GET", f"/v1/summary?{urlencode(today.query)}")] = (
        503,
        b"{}",
        {},
    )
    backend.summaries[""]["active"] = 100
    await entry.runtime_data.async_refresh()
    assert not entry.runtime_data.last_update_success
    assert entry.runtime_data.data == previous
    assert (
        get_state(hass, "sensor.shipments_inbound_delivered_today").state
        == "unavailable"
    )


async def test_presence_shares_exact_ha_day_with_the_app_ui(
    hass: HomeAssistant, backend: Backend, freezer
) -> None:
    await hass.config.async_set_time_zone("America/Los_Angeles")
    freezer.move_to(datetime.fromisoformat("2026-03-08T09:30:00+00:00"))
    backend.responses[("POST", "/v1/integration/presence")] = (204, b"", {})
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            CONF_BACKEND_URL: backend.url,
            CONF_TOKEN: "test-owner-token",
            CONF_ADDON_SLUG: "test_shipments",
        },
    )
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    await entry.runtime_data.async_refresh()
    presence = [
        request
        for request in backend.requests
        if request.path == "/v1/integration/presence"
    ][-1]
    report = json.loads(presence.body)
    summary = [
        request
        for request in backend.requests
        if request.path == "/v1/summary" and "delivered_after" in request.query
    ][-1]
    assert report["delivery_day"] == {
        "date": "2026-03-08",
        "time_zone": "America/Los_Angeles",
        "delivered_after": int(
            datetime.fromisoformat("2026-03-08T08:00:00+00:00").timestamp()
        ),
        "delivered_before": int(
            datetime.fromisoformat("2026-03-09T07:00:00+00:00").timestamp()
        ),
    }
    assert summary.query == {
        "direction": "inbound",
        "delivered_after": str(report["delivery_day"]["delivered_after"]),
        "delivered_before": str(report["delivery_day"]["delivered_before"]),
        "delivered_on": report["delivery_day"]["date"],
    }


@pytest.mark.parametrize("route", ["summary", "shipments"])
async def test_ha_bridge_preserves_delivery_day_bounds(
    hass_client, entry, backend: Backend, route: str
) -> None:
    client = await hass_client()
    bounds = {
        "direction": "inbound",
        "delivered_after": "1792047600",
        "delivered_before": "1792134000",
        "delivered_on": "2026-10-15",
    }
    response = await client.get(f"/api/shipments/v1/{route}", params=bounds)
    assert response.status == 200
    assert backend.requests[-1].query == bounds


async def test_older_backend_does_not_publish_all_time_deliveries_as_today(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    """Keep existing counts while refusing an unsupported day projection."""
    current_version = backend.version
    hass.config_entries.async_update_entry(
        entry, data={**entry.data, CONF_ADDON_SLUG: "test_shipments"}
    )
    backend.responses[("POST", "/v1/integration/presence")] = (204, b"", {})
    backend.version = "0.5.1"
    backend.summaries["inbound"]["delivered"] = 99
    # Older servers ignore unknown bounds and return the all-time total.
    backend.delivery_summary["delivered"] = 99
    backend.requests.clear()
    await entry.runtime_data.async_refresh()
    assert entry.runtime_data.last_update_success
    assert "inbound_delivered_today" not in entry.runtime_data.data
    assert (
        get_state(hass, "sensor.shipments_inbound_delivered_today").state
        == "unavailable"
    )
    assert get_state(hass, "sensor.shipments_active").state == "4"
    assert get_state(hass, "sensor.shipments_inbound_active").state == "3"
    assert get_state(hass, "sensor.shipments_outbound_active").state == "1"
    assert not any("delivered_after" in request.query for request in backend.requests)
    issue = ir.async_get(hass).async_get_issue(DOMAIN, "version_mismatch")
    assert issue is not None
    assert issue.translation_placeholders == {
        "app_version": "0.5.1",
        "integration_version": current_version,
    }
    presence = [
        request
        for request in backend.requests
        if request.path == "/v1/integration/presence"
    ][-1]
    assert "delivery_day" not in json.loads(presence.body)

    backend.version = current_version
    backend.delivery_summary["delivered"] = 2
    backend.requests.clear()
    await entry.runtime_data.async_refresh()
    assert entry.runtime_data.last_update_success
    assert get_state(hass, "sensor.shipments_inbound_delivered_today").state == "2"
    assert not ir.async_get(hass).async_get_issue(DOMAIN, "version_mismatch")
    assert any("delivered_after" in request.query for request in backend.requests)
    presence = [
        request
        for request in backend.requests
        if request.path == "/v1/integration/presence"
    ][-1]
    assert "delivery_day" in json.loads(presence.body)
