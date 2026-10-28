"""Download the native diagnostic file through Home Assistant's admin endpoint."""

import json

import pytest
from homeassistant.config_entries import ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.setup import async_setup_component
from pytest_homeassistant_custom_component.common import MockConfigEntry
from pytest_homeassistant_custom_component.typing import ClientSessionGenerator

from custom_components.shipments.const import (
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
    SENSOR_KEYS,
)

from .conftest import Backend, FixtureShipment


async def download(
    hass: HomeAssistant, hass_client: ClientSessionGenerator, entry: MockConfigEntry
) -> tuple[dict[str, object], str]:
    """Use the public download path, including Core's diagnostic envelope."""
    assert await async_setup_component(hass, "diagnostics", {})
    await hass.async_block_till_done()
    client = await hass_client()
    response = await client.get(f"/api/diagnostics/config_entry/{entry.entry_id}")
    assert response.status == 200
    assert response.content_type == "application/json"
    assert response.headers["Content-Disposition"].startswith("attachment;")
    text = await response.text()
    return json.loads(text)["data"], text


async def test_native_download_contains_only_cached_health(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    entry: MockConfigEntry,
    backend: Backend,
) -> None:
    backend.shipment_rows = [
        FixtureShipment(
            title="PRIVATE-SHIPMENT-TITLE", tracking_number="PRIVATE-NUMBER"
        )
    ]
    await entry.runtime_data.async_refresh()
    # Unknown upstream fields must not be recursively dumped into a support file.
    entry.runtime_data.data["PRIVATE-UNEXPECTED-COUNTER"] = 7
    hass.config_entries.async_update_entry(
        entry,
        title="PRIVATE-CONNECTION-TITLE",
        options={"private_option": "PRIVATE-OPTION"},
    )
    request_count = len(backend.requests)
    data, text = await download(hass, hass_client, entry)
    assert data == {
        "integration": {
            "version": backend.version,
            "entry_state": "loaded",
            "connection": "standalone",
        },
        "coordinator": {
            "available": True,
            "last_update_success": True,
            "last_error_type": None,
            "update_interval_seconds": 60.0,
            "event_stream_available": True,
        },
        "backend": {
            "last_observed_version": backend.version,
            "last_observed_versions_match": True,
        },
        "summary": {
            "current": True,
            "counts": {key: entry.runtime_data.data[key] for key in SENSOR_KEYS},
        },
        "repairs": {
            "version_mismatch": False,
            "event_stream_unavailable": False,
            "event_history_reset": False,
        },
    }
    for private in (
        "PRIVATE-",
        "test-owner-token",
        "Office supplies",
        "Private merchant",
        "Private recipient",
        backend.url,
        "Authorization",
        "Bearer",
    ):
        assert private not in text
    assert len(backend.requests) == request_count


async def test_failed_refresh_retains_explicitly_stale_counts(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    entry: MockConfigEntry,
    backend: Backend,
) -> None:
    previous = entry.runtime_data.data.copy()
    backend.responses[("GET", "/v1/summary")] = (
        503,
        b'{"error":"PRIVATE-PROVIDER-ERROR", "tracking":"PRIVATE-NUMBER"}',
        {"X-Private-Header": "PRIVATE-HEADER"},
    )
    await entry.runtime_data.async_refresh()
    request_count = len(backend.requests)
    data, text = await download(hass, hass_client, entry)
    assert data["summary"] == {"current": False, "counts": previous}
    assert data["coordinator"] == {
        "available": False,
        "last_update_success": False,
        "last_error_type": "UpdateFailed",
        "update_interval_seconds": 60.0,
        "event_stream_available": False,
    }
    assert "PRIVATE-" not in text
    assert len(backend.requests) == request_count


async def test_exception_messages_and_causes_are_not_downloaded(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    entry: MockConfigEntry,
    backend: Backend,
) -> None:
    failure = ValueError("PRIVATE-TOKEN at http://PRIVATE-HOST/PRIVATE-PATH")
    failure.__cause__ = OSError("PRIVATE-PROVIDER-RESPONSE")
    entry.runtime_data.async_set_update_error(failure)
    request_count = len(backend.requests)
    data, text = await download(hass, hass_client, entry)
    assert data["coordinator"] == {
        "available": False,
        "last_update_success": False,
        "last_error_type": "ValueError",
        "update_interval_seconds": 60.0,
        "event_stream_available": False,
    }
    assert "PRIVATE-" not in text
    assert len(backend.requests) == request_count


@pytest.mark.parametrize("status", [401, 403, 503])
async def test_failed_version_read_preserves_last_observation_without_response_data(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    entry: MockConfigEntry,
    backend: Backend,
    status: int,
) -> None:
    known_version = backend.version
    backend.responses[("GET", "/v1/system")] = (
        status,
        b'{"message":"PRIVATE-URL http://private.invalid PRIVATE-CREDENTIAL"}',
        {"Set-Cookie": "PRIVATE-COOKIE"},
    )
    await entry.runtime_data.async_refresh()
    request_count = len(backend.requests)
    data, text = await download(hass, hass_client, entry)
    assert data["backend"] == {
        "last_observed_version": known_version,
        "last_observed_versions_match": True,
    }
    assert data["coordinator"] == {
        "available": False,
        "last_update_success": False,
        "last_error_type": "ConfigEntryAuthFailed" if status < 500 else "UpdateFailed",
        "update_interval_seconds": 60.0,
        "event_stream_available": False,
    }
    assert "PRIVATE-" not in text
    assert len(backend.requests) == request_count


async def test_mixed_versions_keep_unavailable_day_count_absent(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    entry: MockConfigEntry,
    backend: Backend,
) -> None:
    backend.version = "0.0.1"
    await entry.runtime_data.async_refresh()
    request_count = len(backend.requests)
    data, _ = await download(hass, hass_client, entry)
    assert data["backend"] == {
        "last_observed_version": "0.0.1",
        "last_observed_versions_match": False,
    }
    assert data["summary"] == {
        "current": True,
        "counts": {
            key: entry.runtime_data.data[key]
            for key in SENSOR_KEYS
            if key not in {"inbound_delivered_today", "inbound_due_today"}
        },
    }
    assert data["repairs"] == {
        "version_mismatch": True,
        "event_stream_unavailable": False,
        "event_history_reset": False,
    }
    assert len(backend.requests) == request_count


async def test_real_zero_counts_remain_zero(
    hass: HomeAssistant, hass_client: ClientSessionGenerator, backend: Backend
) -> None:
    for summary in [*backend.summaries.values(), backend.delivery_summary]:
        summary.update(dict.fromkeys(summary, 0))
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={CONF_BACKEND_URL: backend.url, CONF_TOKEN: "PRIVATE-TOKEN"},
    )
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    data, _ = await download(hass, hass_client, entry)
    assert data["summary"] == {
        "current": True,
        "counts": dict.fromkeys(SENSOR_KEYS, 0),
    }


@pytest.mark.parametrize("failed_setup", [False, True])
async def test_unloaded_or_never_connected_entry_has_unknown_health(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    backend: Backend,
    failed_setup: bool,
) -> None:
    entry = MockConfigEntry(
        domain=DOMAIN,
        title="PRIVATE-ENTRY-TITLE",
        data={CONF_BACKEND_URL: backend.url, CONF_TOKEN: "PRIVATE-TOKEN"},
    )
    entry.add_to_hass(hass)
    if failed_setup:
        backend.responses[("GET", "/v1/system")] = (503, b"PRIVATE-ERROR", {})
        assert not await hass.config_entries.async_setup(entry.entry_id)
        assert entry.state is ConfigEntryState.SETUP_RETRY
    else:
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done(wait_background_tasks=True)
        assert await hass.config_entries.async_unload(entry.entry_id)
        assert entry.state is ConfigEntryState.NOT_LOADED
    request_count = len(backend.requests)
    data, text = await download(hass, hass_client, entry)
    assert data["coordinator"] == {
        "available": False,
        "last_update_success": None,
        "last_error_type": None,
        "update_interval_seconds": None,
        "event_stream_available": None,
    }
    assert data["summary"] == {"current": False, "counts": None}
    assert data["backend"] == {
        "last_observed_version": None,
        "last_observed_versions_match": None,
    }
    assert "PRIVATE-" not in text
    assert len(backend.requests) == request_count


async def test_native_download_requires_admin(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_client_no_auth: ClientSessionGenerator,
    hass_user_token: str,
    entry: MockConfigEntry,
    backend: Backend,
) -> None:
    assert await async_setup_component(hass, "diagnostics", {})
    await hass.async_block_till_done()
    request_count = len(backend.requests)
    user = await hass_client(hass_user_token)
    assert (await user.get("/api/")).status == 200
    for client in (
        await hass_client_no_auth(),
        user,
    ):
        response = await client.get(f"/api/diagnostics/config_entry/{entry.entry_id}")
        assert response.status == 401
    assert len(backend.requests) == request_count
